//! Platform fast paths for index scans.
//!
//! Strategy per platform:
//!
//! * Windows: attempt an MFT / USN-journal scan via
//!   [`windows::mft_scan_attempt`] (`FSCTL_ENUM_USN_DATA`). Opening a volume
//!   handle requires elevation; when elevation is missing, the journal is
//!   unhealthy, or any Win32 call fails, entry collection falls back to the
//!   tuned `jwalk` walk ([`crate::scan_roots`]) and the degradation is
//!   reported via [`windows::ScanHealth`]. Incremental updates use the
//!   [`windows::UsnCursor`] delta stub: any journal wrap/truncation maps to
//!   [`windows::UsnDeltaOutcome::RescanRequired`], which the caller serves
//!   with a full rescan.
//! * Linux: keep the tuned `jwalk` walk and handle `inotify` watch-limit
//!   exhaustion ([`notify::ErrorKind::MaxFilesWatch`] / `ENOSPC`) by falling
//!   back to interval polling ([`linux::start_watch_or_poll`]).
//!
//! # UNC and long-path behavior (Windows)
//!
//! * UNC paths (`\\server\share\...`, including the `//server/share` slash
//!   spelling) are never enumerated through the MFT path: a volume handle
//!   cannot be opened for a remote share, so
//!   [`windows::mft_scan_attempt`] returns
//!   [`windows::MftFallbackReason::NonLocalVolume`] and the caller uses the
//!   `jwalk` fallback, which reads UNC trees through normal Win32 path APIs.
//!   See [`is_unc_path`].
//! * Long paths (beyond `MAX_PATH`, 260 chars): the Rust standard library
//!   supports them when the path carries the verbatim `\\?\` prefix
//!   (extended-length paths), e.g. `\\?\C:\very\...\file` or
//!   `\\?\UNC\server\share\...`. The `jwalk` fallback passes paths through
//!   to the OS unchanged, so over-long roots keep working as long as the
//!   caller (or the stored index rows) preserve the prefix. Use
//!   [`strip_verbatim_prefix`] only for display; never for opening files,
//!   or the long path will fail with `ERROR_PATH_NOT_FOUND`.
//! * The MFT probe itself always targets a local volume (`\\.\X:`) derived
//!   from the scan root's drive letter, so it is unaffected by per-path
//!   length limits.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{PollHandle, ScanOptions, WatcherStub};

/// True for UNC paths: `\\server\share\...` (and the `//server/share`
/// slash spelling, which Windows accepts and `jwalk` walks verbatim).
///
/// Such roots always use the `jwalk` fallback; see the
/// [module-level documentation](self) for why the MFT path cannot serve
/// them.
pub fn is_unc_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    // Verbatim-extended UNC: `\\?\UNC\server\share`.
    if s.starts_with(r"\\?\UNC\") || s.starts_with(r"\\?\unc\") {
        return true;
    }
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(a), Some(b)) if (a == '\\' || a == '/') && (b == '\\' || b == '/') => {
            // `\\?\...` verbatim paths are drive-qualified, not UNC.
            !s.starts_with(r"\\?\") && !s.starts_with(r"//?/")
        }
        _ => false,
    }
}

/// Strip the verbatim `\\?\` (or `\\?\UNC\`) prefix for display purposes.
///
/// The returned path must not be used to open files: dropping the prefix
/// re-applies the `MAX_PATH` limit. Index storage and scanning keep the
/// original spelling.
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

/// True when a `notify` watcher error means the OS file-watch limit was hit.
///
/// On Linux this is `inotify` `max_user_watches` exhaustion, surfaced by
/// `notify` as [`notify::ErrorKind::MaxFilesWatch`] or as an I/O error with
/// `ENOSPC` ("No space left on device"). Either spelling triggers the poll
/// fallback in [`start_watch_or_poll`].
pub fn is_watch_limit_error(err: &notify::Error) -> bool {
    match &err.kind {
        notify::ErrorKind::MaxFilesWatch => true,
        notify::ErrorKind::Io(io) => {
            // ENOSPC (28) is what the kernel returns when
            // /proc/sys/fs/inotify/max_user_watches is exceeded.
            io.raw_os_error() == Some(28) || io.to_string().contains("No space left on device")
        }
        _ => false,
    }
}

/// Live index feed: either an OS watcher or, when watch registration fails
/// with a watch-limit error, an interval poll thread.
///
/// Constructed by [`start_watch_or_poll`].
pub enum WatchOrPoll {
    /// OS watcher active (inotify on Linux, ReadDirectoryChanges on
    /// Windows). Drain via [`WatcherStub::drain_events`] /
    /// [`WatcherStub::apply_pending`].
    Watch(WatcherStub),
    /// Watch limit (or another registration failure) hit: the index is kept
    /// fresh by [`crate::spawn_poll_thread`] instead.
    Poll(PollHandle),
}

impl WatchOrPoll {
    /// True when the poll fallback is active (watch limit exhausted).
    pub fn is_poll_fallback(&self) -> bool {
        matches!(self, WatchOrPoll::Poll(_))
    }

    /// Stop the poll thread, if any. A live OS watcher stops when dropped.
    pub fn stop(&self) {
        if let WatchOrPoll::Poll(handle) = self {
            handle.stop();
        }
    }

    /// Stop the poll thread and wait for it to exit (no-op for watchers).
    pub fn stop_and_join(self) {
        if let WatchOrPoll::Poll(handle) = self {
            handle.stop_and_join();
        }
    }
}

/// Watch `options.roots` for incremental indexing, falling back to interval
/// polling when the OS refuses the watch registration.
///
/// * Tries [`WatcherStub::start_with_db`] first.
/// * On a watch-limit error (see [`is_watch_limit_error`]) — or any other
///   registration failure — starts [`crate::spawn_poll_thread`] over the
///   same roots so the index stays fresh in a degraded-but-live state.
///
/// Never fails on registration errors: the poll fallback owns no OS watch
/// handles, so only a thread-spawn panic (which aborts via `expect` with a
/// message) can escape.
pub fn start_watch_or_poll(
    options: &ScanOptions,
    db_path: PathBuf,
    poll_interval: Duration,
) -> WatchOrPoll {
    match WatcherStub::start_with_db(options, db_path.clone()) {
        Ok(watcher) => WatchOrPoll::Watch(watcher),
        Err(_) => WatchOrPoll::Poll(crate::spawn_poll_thread(
            db_path,
            options.clone(),
            poll_interval,
        )),
    }
}

/// Windows fast path: MFT / USN-journal scan attempt with elevation gating
/// and health-degraded fallback to the tuned `jwalk` walk.
///
/// Compiles and runs without admin: every privileged call is attempted at
/// runtime and mapped to [`MftFallbackReason`], never required at build
/// time.
#[cfg(windows)]
pub mod windows {
    use std::ffi::c_void;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Path, PathBuf};

    use crate::{ScanOptions, ScanResult};

    /// `FSCTL_ENUM_USN_DATA`: enumerate USN-journal records on a volume
    /// handle. Input is [`MftEnumData`]; output is a byte stream of
    /// `USN_RECORD_V2`/`USN_RECORD_V3` structs.
    pub const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00B3;
    /// `FSCTL_QUERY_USN_JOURNAL`: report journal health/identity.
    /// Input: none; output: [`UsnJournalData`].
    pub const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00F4;

    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const ERROR_ACCESS_DENIED: u32 = 5;
    /// The driver signals end-of-enumeration with this instead of an empty
    /// batch on large volumes: losing it as an error would discard a full
    /// table read at the finish line.
    const ERROR_HANDLE_EOF: u32 = 38;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *const c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: *const c_void,
        ) -> *mut c_void;
        fn DeviceIoControl(
            h_device: *const c_void,
            dw_io_control_code: u32,
            lp_in_buffer: *const c_void,
            n_in_buffer_size: u32,
            lp_out_buffer: *mut c_void,
            n_out_buffer_size: u32,
            lp_bytes_returned: *mut u32,
            lp_overlapped: *mut c_void,
        ) -> i32;
        fn CloseHandle(h_object: *const c_void) -> i32;
        fn GetLastError() -> u32;
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn IsUserAnAdmin() -> i32;
    }

    /// Input for `FSCTL_ENUM_USN_DATA`: where journal enumeration starts.
    #[repr(C)]
    struct MftEnumData {
        start_file_reference_number: u64,
        low_usn: i64,
        high_usn: i64,
    }

    /// Output of `FSCTL_QUERY_USN_JOURNAL` (fixed prefix; the trailing
    /// variable fields are not needed here).
    #[repr(C)]
    struct UsnJournalData {
        usn_journal_id: u64,
        first_usn: i64,
        next_usn: i64,
        lowest_valid_usn: i64,
        max_usn: i64,
        maximum_size: u64,
        allocation_delta: u64,
    }

    /// RAII volume handle: closed via `CloseHandle` on drop.
    struct VolumeHandle(*mut c_void);

    impl Drop for VolumeHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // Best-effort: nothing useful to do with a close failure.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }

    fn wide_null(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// True when the current process runs elevated (admin).
    ///
    /// Pure runtime check (`IsUserAnAdmin`); safe to call without admin —
    /// it simply returns `false` — and requires nothing at compile time.
    pub fn is_elevated() -> bool {
        unsafe { IsUserAnAdmin() != 0 }
    }

    /// Why the MFT path could not serve a scan (entry collection falls back
    /// to the tuned `jwalk` walk in every case).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum MftFallbackReason {
        /// Process is not elevated, so the volume handle cannot be opened.
        NotElevated,
        /// Volume open rejected (`ERROR_ACCESS_DENIED` despite the
        /// elevation probe passing, e.g. UAC / policy edge cases).
        AccessDenied,
        /// Root is not a local volume (UNC share, mapped-without-volume
        /// path, or unparseable prefix): a volume handle cannot exist for
        /// it. Carries the offending root.
        NonLocalVolume(String),
        /// `FSCTL_QUERY_USN_JOURNAL` failed (journal disabled, removed, or
        /// the filesystem is not NTFS). Carries the Win32 error.
        JournalUnhealthy(u32),
        /// `FSCTL_ENUM_USN_DATA` failed. Carries `GetLastError()`.
        EnumerateFailed(u32),
    }

    impl std::fmt::Display for MftFallbackReason {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                MftFallbackReason::NotElevated => write!(f, "process is not elevated"),
                MftFallbackReason::AccessDenied => write!(f, "volume open: access denied"),
                MftFallbackReason::NonLocalVolume(root) => {
                    write!(f, "non-local volume root: {root}")
                }
                MftFallbackReason::JournalUnhealthy(e) => {
                    write!(f, "USN journal unhealthy (Win32 error {e})")
                }
                MftFallbackReason::EnumerateFailed(e) => {
                    write!(f, "FSCTL_ENUM_USN_DATA failed (Win32 error {e})")
                }
            }
        }
    }

    /// Successful MFT/USN probe: the journal answered.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct MftProbeOk {
        /// Volume probed, e.g. `\\.\C:`.
        pub volume: String,
        /// USN records observed in the bounded probe batches.
        pub records_seen: u64,
        /// Journal identity (feeds [`UsnCursor`]).
        pub journal_id: u64,
        /// Journal write head at probe time (feeds [`UsnCursor`]).
        pub next_usn: i64,
    }

    /// Overall scan health from the platform point of view.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ScanHealth {
        /// The MFT/USN probe answered. Name-table entry sourcing lives in
        /// [`mft_search`]; the probe-only [`fast_scan_or_fallback`] still
        /// collects through the tuned `jwalk` walk.
        Healthy,
        /// The MFT path is unavailable; entries come from the `jwalk`
        /// fallback. Carries the human-readable reason.
        Degraded(String),
    }

    /// Derive the `\\.\X:` volume device path for a scan root.
    ///
    /// Returns `None` for UNC roots, relative roots, and anything without
    /// a disk-letter prefix — all of which must use the `jwalk` fallback.
    pub fn volume_for_root(root: &Path) -> Option<String> {
        match root.components().next()? {
            Component::Prefix(prefix) => match prefix.kind() {
                std::path::Prefix::Disk(d) | std::path::Prefix::VerbatimDisk(d) => {
                    Some(format!(r"\\.\{}:", (d as char).to_ascii_uppercase()))
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn open_volume(volume: &str) -> Result<VolumeHandle, u32> {
        let wide = wide_null(OsStr::new(volume));
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null(),
            )
        };
        if handle.is_null() || handle == usize::MAX as *mut c_void {
            Err(unsafe { GetLastError() })
        } else {
            Ok(VolumeHandle(handle))
        }
    }

    fn query_journal(handle: &VolumeHandle) -> Result<UsnJournalData, u32> {
        let mut out = UsnJournalData {
            usn_journal_id: 0,
            first_usn: 0,
            next_usn: 0,
            lowest_valid_usn: 0,
            max_usn: 0,
            maximum_size: 0,
            allocation_delta: 0,
        };
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle.0,
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                (&mut out as *mut UsnJournalData).cast(),
                std::mem::size_of::<UsnJournalData>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(unsafe { GetLastError() })
        } else {
            Ok(out)
        }
    }

    /// Issue one bounded `FSCTL_ENUM_USN_DATA` pass and count the records.
    ///
    /// Reads at most `max_batches` 64 KiB batches starting at USN 0 so a
    /// huge journal cannot stall a scan; record headers are walked with
    /// `RecordLength` strides and `from_ne_bytes` copies (no unaligned
    /// reads). Returns `(records_seen, journal_id, next_usn)`.
    fn enumerate_usn_records(
        handle: &VolumeHandle,
        journal: &UsnJournalData,
        max_batches: usize,
    ) -> Result<(u64, u64, i64), u32> {
        let mut input = MftEnumData {
            start_file_reference_number: 0,
            low_usn: 0,
            high_usn: i64::MAX,
        };
        let mut out = vec![0u8; 64 * 1024];
        let mut records = 0u64;
        for _ in 0..max_batches.max(1) {
            let mut returned = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    handle.0,
                    FSCTL_ENUM_USN_DATA,
                    (&input as *const MftEnumData).cast(),
                    std::mem::size_of::<MftEnumData>() as u32,
                    out.as_mut_ptr().cast(),
                    out.len() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(unsafe { GetLastError() });
            }
            if returned <= 8 {
                break;
            }
            // Bytes [0..8) are the next-enumeration FRN; records follow.
            let next_frn = u64::from_ne_bytes(out[0..8].try_into().unwrap_or([0; 8]));
            let mut off = 8usize;
            let end = returned as usize;
            while off + 4 <= end {
                let len =
                    u32::from_ne_bytes(out[off..off + 4].try_into().unwrap_or([0; 4])) as usize;
                if len < 4 || off + len > end {
                    break;
                }
                records += 1;
                off += len;
            }
            if next_frn == 0 || next_frn == input.start_file_reference_number {
                break;
            }
            input.start_file_reference_number = next_frn;
        }
        Ok((records, journal.usn_journal_id, journal.next_usn))
    }

    /// Low 48 bits of a file reference number: the MFT segment index. The
    /// high 16 bits are a sequence number that changes when the slot is
    /// reused, so parent links resolve through the segment index.
    const FRN_INDEX_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
    /// `FILE_ATTRIBUTE_DIRECTORY` / `FILE_ATTRIBUTE_HIDDEN` as stored in
    /// USN records.
    const ATTR_DIRECTORY: u32 = 0x10;
    const ATTR_HIDDEN: u32 = 0x02;
    /// FRN segment of the volume root directory. Shared with the delta
    /// applier for parent-less rows.
    pub(crate) const ROOT_FRN: u64 = 5;
    /// Cap on parent hops while resolving one path: bounds corrupted or
    /// cyclic parent links instead of looping forever.
    const MAX_RESOLVE_DEPTH: usize = 1024;

    /// One MFT record's identity triple: everything the fast path needs
    /// before parent FRNs resolve to full paths. No filesystem I/O.
    #[derive(Debug, Clone)]
    pub struct MftNameEntry {
        /// MFT segment index ([`FRN_INDEX_MASK`] applied).
        pub frn: u64,
        /// Parent's MFT segment index.
        pub parent_frn: u64,
        /// File name (UTF-16 decoded lossily).
        pub name: String,
        /// `FileAttributes` from the USN record.
        pub attributes: u32,
    }

    impl MftNameEntry {
        /// True for directory records (`FILE_ATTRIBUTE_DIRECTORY`).
        pub fn is_dir(&self) -> bool {
            self.attributes & ATTR_DIRECTORY != 0
        }
        /// True for hidden records (`FILE_ATTRIBUTE_HIDDEN`).
        pub fn is_hidden_attr(&self) -> bool {
            self.attributes & ATTR_HIDDEN != 0
        }
    }

    /// `USN_REASON` bits the delta applier acts on. Full mask list lives
    /// in `winnt.h`; only the create/delete/rename bits change index
    /// membership, everything else refreshes in place.
    pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
    /// `USN_REASON_FILE_DELETE`: the path is gone, remove it (and any
    /// subtree beneath it) from the index.
    pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
    /// `USN_REASON_RENAME_OLD_NAME`: this record names the path as it was
    /// before the rename — remove it; the paired `RENAME_NEW_NAME` record
    /// re-adds the new path.
    pub const USN_REASON_RENAME_OLD: u32 = 0x0000_1000;
    /// `USN_REASON_RENAME_NEW_NAME`: the renamed-in path, upserted like a
    /// create.
    pub const USN_REASON_RENAME_NEW: u32 = 0x0000_2000;

    /// What one journal record means for index membership.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DeltaKind {
        /// Refresh the resolved path in place (creates, writes, and the
        /// rename-new side all land here).
        Upsert,
        /// Drop the resolved path (and any subtree) from the index.
        Remove,
    }

    /// Map a USN reason mask to index membership: deletes and the
    /// rename-old side remove, everything else refreshes in place.
    pub fn classify_delta(reason: u32) -> DeltaKind {
        if reason & (USN_REASON_FILE_DELETE | USN_REASON_RENAME_OLD) != 0 {
            DeltaKind::Remove
        } else {
            DeltaKind::Upsert
        }
    }

    /// Parse one `USN_RECORD_V2` / `USN_RECORD_V3` at the head of `buf`.
    ///
    /// Returns the entry plus its `RecordLength` stride on success;
    /// `None` for truncated buffers, unknown versions, or out-of-range
    /// name fields. Pure function over bytes: no handle, no elevation.
    fn parse_usn_record(buf: &[u8]) -> Option<(MftNameEntry, usize)> {
        let (entry, _, _, len) = parse_usn_record_full(buf)?;
        Some((entry, len))
    }

    /// Full record parse: entry plus the record's USN, reason mask, and
    /// `RecordLength` stride. [`parse_usn_record`] and the delta reader
    /// share this so the two layouts cannot drift apart.
    fn parse_usn_record_full(buf: &[u8]) -> Option<(MftNameEntry, i64, u32, usize)> {
        if buf.len() < 60 {
            return None;
        }
        let len = u32::from_le_bytes(buf[0..4].try_into().ok()?) as usize;
        if len < 60 || len > buf.len() {
            return None;
        }
        let major = u16::from_le_bytes(buf[4..6].try_into().ok()?);
        let rec = &buf[..len];
        // (frn, parent_frn, usn, reason, attributes, name_offset,
        // name_len_bytes)
        let (frn, parent_frn, usn, reason, attributes, name_off, name_len) = match major {
            2 => {
                let frn = u64::from_le_bytes(rec.get(8..16)?.try_into().ok()?) & FRN_INDEX_MASK;
                let parent = u64::from_le_bytes(rec.get(16..24)?.try_into().ok()?) & FRN_INDEX_MASK;
                let usn = i64::from_le_bytes(rec.get(24..32)?.try_into().ok()?);
                let reason = u32::from_le_bytes(rec.get(40..44)?.try_into().ok()?);
                let attributes = u32::from_le_bytes(rec.get(52..56)?.try_into().ok()?);
                let name_len = u16::from_le_bytes(rec.get(56..58)?.try_into().ok()?) as usize;
                let name_off = u16::from_le_bytes(rec.get(58..60)?.try_into().ok()?) as usize;
                (frn, parent, usn, reason, attributes, name_off, name_len)
            }
            3 => {
                // V3 reference numbers are 128-bit; the low 64 bits carry
                // the segment index used for parent resolution.
                let frn = u64::from_le_bytes(rec.get(8..16)?.try_into().ok()?) & FRN_INDEX_MASK;
                let parent = u64::from_le_bytes(rec.get(24..32)?.try_into().ok()?) & FRN_INDEX_MASK;
                let usn = i64::from_le_bytes(rec.get(40..48)?.try_into().ok()?);
                let reason = u32::from_le_bytes(rec.get(56..60)?.try_into().ok()?);
                let attributes = u32::from_le_bytes(rec.get(68..72)?.try_into().ok()?);
                let name_len = u16::from_le_bytes(rec.get(72..74)?.try_into().ok()?) as usize;
                let name_off = u16::from_le_bytes(rec.get(74..76)?.try_into().ok()?) as usize;
                (frn, parent, usn, reason, attributes, name_off, name_len)
            }
            _ => return None,
        };
        let name_end = name_off.checked_add(name_len)?;
        let name_bytes = rec.get(name_off..name_end)?;
        if name_bytes.len() % 2 != 0 {
            return None;
        }
        let units: Vec<u16> = name_bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let name = String::from_utf16_lossy(&units);
        Some((
            MftNameEntry {
                frn,
                parent_frn,
                name,
                attributes,
            },
            usn,
            reason,
            len,
        ))
    }

    /// Parse one enumeration batch (bytes after the 8-byte next-FRN
    /// header) into name records.
    ///
    /// Corrupt records are skipped and counted so one odd record can
    /// never abort a volume scan. Takes the slot buffer by value and
    /// hands it back untouched (still full capacity) with the result, so
    /// the double-buffered loop below performs zero copies and zero
    /// allocations in steady state. Only `end` bytes are valid.
    fn parse_batch(batch: Vec<u8>, end: usize) -> (Vec<MftNameEntry>, u64, Vec<u8>) {
        let mut entries = Vec::new();
        let mut skipped = 0u64;
        let mut off = 8usize;
        while off + 4 <= end {
            match parse_usn_record(&batch[off..end]) {
                Some((entry, len)) => {
                    entries.push(entry);
                    off += len;
                }
                None => {
                    // Advance by the declared length when sane so one
                    // corrupt record does not end the batch.
                    let len = u32::from_le_bytes(batch[off..off + 4].try_into().unwrap_or([0; 4]))
                        as usize;
                    skipped += 1;
                    if len < 4 || off + len > end {
                        break;
                    }
                    off += len;
                }
            }
        }
        (entries, skipped, batch)
    }

    /// Fold one finished parse worker back: records appended in order,
    /// scratch buffer recycled into its slot.
    fn fold_worker(
        worker: &mut Option<std::thread::ScopedJoinHandle<'_, (Vec<MftNameEntry>, u64, Vec<u8>)>>,
        slot_buf: &mut Vec<u8>,
        entries: &mut Vec<MftNameEntry>,
        skipped: &mut u64,
    ) {
        if let Some(handle) = worker.take() {
            let (mut batch_entries, batch_skipped, buf) =
                handle.join().expect("parse batch panicked");
            entries.append(&mut batch_entries);
            *skipped += batch_skipped;
            *slot_buf = buf;
        }
    }

    /// Enumerate every MFT name record on an open volume handle.
    ///
    /// Unlike [`enumerate_usn_records`], this loop is unbounded by design:
    /// it pages until the enumeration FRN repeats or returns zero, because
    /// a partial table cannot serve path resolution. Each batch parses on
    /// a worker thread while the next ioctl is already in flight
    /// (depth-1 pipeline), and order is preserved by draining before
    /// queueing the next batch.
    fn enumerate_mft_names(handle: &VolumeHandle) -> Result<(Vec<MftNameEntry>, u64), u32> {
        let mut input = MftEnumData {
            start_file_reference_number: 0,
            low_usn: 0,
            high_usn: i64::MAX,
        };
        let mut entries = Vec::new();
        let mut skipped = 0u64;
        // Two alternating scratch buffers: while one batch parses on a
        // worker thread, the next ioctl already fills the other, so
        // parsing overlaps kernel round-trips with zero copies. A slot's
        // worker is always folded (returning its buffer) before that slot
        // is reused, which also preserves batch order — issuing an ioctl
        // into a still-empty slot would silently truncate the table, so
        // the length below is asserted, not assumed. Both live inside
        // the scope because workers borrow it.
        const SLOT_BYTES: usize = 4 << 20;
        std::thread::scope(|s| {
            let mut bufs = [vec![0u8; SLOT_BYTES], vec![0u8; SLOT_BYTES]];
            let mut workers: Vec<
                Option<std::thread::ScopedJoinHandle<'_, (Vec<MftNameEntry>, u64, Vec<u8>)>>,
            > = vec![None, None];
            let mut cur = 0usize;
            loop {
                // Fold this slot's worker first: it was spawned two
                // iterations ago, so the join only collects an already
                // finished parse (returning its buffer), while the other
                // slot's parse runs concurrently with the ioctl below.
                // This ordering both overlaps and preserves batch order.
                if workers[cur].is_some() {
                    let slot = &mut bufs[cur];
                    fold_worker(&mut workers[cur], slot, &mut entries, &mut skipped);
                }
                debug_assert_eq!(
                    bufs[cur].len(),
                    SLOT_BYTES,
                    "MFT slot buffer missing at ioctl time"
                );
                let out = &mut bufs[cur];
                let mut returned = 0u32;
                let ok = unsafe {
                    DeviceIoControl(
                        handle.0,
                        FSCTL_ENUM_USN_DATA,
                        (&input as *const MftEnumData).cast(),
                        std::mem::size_of::<MftEnumData>() as u32,
                        out.as_mut_ptr().cast(),
                        out.len() as u32,
                        &mut returned,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 {
                    let err = unsafe { GetLastError() };
                    if err == ERROR_HANDLE_EOF {
                        // Clean end of the table (see ERROR_HANDLE_EOF).
                        break;
                    }
                    return Err(err);
                }
                if returned <= 8 {
                    break;
                }
                let next_frn = u64::from_le_bytes(out[0..8].try_into().unwrap_or([0; 8]));
                let end = returned as usize;
                let batch = std::mem::replace(&mut bufs[cur], Vec::new());
                workers[cur] = Some(s.spawn(move || parse_batch(batch, end)));
                if next_frn == 0 || next_frn == input.start_file_reference_number {
                    break;
                }
                input.start_file_reference_number = next_frn;
                cur ^= 1;
            }
            // Reclaim the in-flight batch(es); slot order no longer
            // matters because no further batches follow.
            for slot in 0..workers.len() {
                if workers[slot].is_some() {
                    let buf = &mut bufs[slot];
                    fold_worker(&mut workers[slot], buf, &mut entries, &mut skipped);
                }
            }
            Ok::<(), u32>(())
        })?;
        Ok((entries, skipped))
    }

    /// Enumerate every MFT name record on `volume` (`\\.\X:`).
    ///
    /// Gated on elevation like the probe (volume handles require it); any
    /// Win32 failure maps to [`MftFallbackReason`] so callers fall back to
    /// the tuned walk. No USN-journal health check: enumeration reads the
    /// table itself and works whether or not a journal exists.
    pub fn mft_enumerate_volume(volume: &str) -> Result<Vec<MftNameEntry>, MftFallbackReason> {
        if !is_elevated() {
            return Err(MftFallbackReason::NotElevated);
        }
        let handle = open_volume(volume).map_err(|e| {
            if e == ERROR_ACCESS_DENIED {
                MftFallbackReason::AccessDenied
            } else {
                MftFallbackReason::EnumerateFailed(e)
            }
        })?;
        let (entries, _skipped) =
            enumerate_mft_names(&handle).map_err(MftFallbackReason::EnumerateFailed)?;
        Ok(entries)
    }

    /// One resolved MFT path plus the metadata the table already knew.
    #[derive(Debug, Clone)]
    pub struct MftResolved {
        /// Full volume-anchored path (e.g. `C:\dir\file.txt`).
        pub path: PathBuf,
        /// From the record's `FILE_ATTRIBUTE_DIRECTORY`.
        pub is_dir: bool,
        /// From the record's hidden bit or a dotted file name.
        pub hidden: bool,
    }

    /// Segment-indexed lookup over MFT entries: sorted once, shared by all
    /// resolution threads. Also shared with the delta applier
    /// (`crate::apply_usn_deltas`).
    pub(crate) fn build_frn_lookup(entries: &[MftNameEntry]) -> Vec<(u64, usize)> {
        let mut by_frn: Vec<(u64, usize)> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.frn, i))
            .collect();
        by_frn.sort_unstable_by_key(|&(frn, _)| frn);
        by_frn
    }

    /// Find one entry by segment index through a [`build_frn_lookup`]
    /// table.
    fn find_frn<'a>(
        lookup: &[(u64, usize)],
        entries: &'a [MftNameEntry],
        frn: u64,
    ) -> Option<&'a MftNameEntry> {
        lookup
            .binary_search_by_key(&frn, |&(f, _)| f)
            .ok()
            .map(|pos| &entries[lookup[pos].1])
    }

    /// Resolve one record to a volume-anchored path.
    ///
    /// `None` means orphan (missing parent, depth overflow): the caller
    /// counts it and moves on. The volume root record resolves to
    /// `volume_root` itself. Shared with the delta applier.
    pub(crate) fn resolve_one(
        lookup: &[(u64, usize)],
        entries: &[MftNameEntry],
        volume_root: &Path,
        entry: &MftNameEntry,
    ) -> Option<MftResolved> {
        if entry.frn == ROOT_FRN {
            return Some(MftResolved {
                path: volume_root.to_path_buf(),
                is_dir: true,
                hidden: false,
            });
        }
        let mut components: Vec<&str> = vec![entry.name.as_str()];
        let mut cursor = entry.parent_frn;
        for _ in 0..MAX_RESOLVE_DEPTH {
            if cursor == ROOT_FRN {
                let mut path = volume_root.to_path_buf();
                for name in components.iter().rev() {
                    path.push(name);
                }
                return Some(MftResolved {
                    path,
                    is_dir: entry.is_dir(),
                    hidden: entry.is_hidden_attr() || entry.name.starts_with('.'),
                });
            }
            match find_frn(lookup, entries, cursor) {
                Some(parent) => {
                    components.push(parent.name.as_str());
                    cursor = parent.parent_frn;
                }
                None => return None,
            }
        }
        None
    }

    /// Resolve MFT name records to volume-anchored full paths.
    ///
    /// Single-threaded convenience over [`resolve_one`] with a shared
    /// lookup; the parallel search below fans the same work out across
    /// threads. Records whose parent is missing (reused slots, torn
    /// state) are dropped and counted as orphans; the volume root record
    /// itself resolves to `volume_root`. Returns `(resolved, orphans)`.
    pub fn resolve_mft_paths(
        volume_root: &Path,
        entries: &[MftNameEntry],
    ) -> (Vec<MftResolved>, u64) {
        let lookup = build_frn_lookup(entries);
        let mut out = Vec::new();
        let mut orphans = 0u64;
        for entry in entries {
            match resolve_one(&lookup, entries, volume_root, entry) {
                Some(resolved) => out.push(resolved),
                None => orphans += 1,
            }
        }
        (out, orphans)
    }

    /// Probe one local volume: open it, check journal health, and run a
    /// bounded `FSCTL_ENUM_USN_DATA` pass.
    pub fn mft_probe_volume(volume: &str) -> Result<MftProbeOk, MftFallbackReason> {
        if !is_elevated() {
            return Err(MftFallbackReason::NotElevated);
        }
        let handle = open_volume(volume).map_err(|e| {
            if e == ERROR_ACCESS_DENIED {
                MftFallbackReason::AccessDenied
            } else {
                MftFallbackReason::JournalUnhealthy(e)
            }
        })?;
        let journal = query_journal(&handle).map_err(MftFallbackReason::JournalUnhealthy)?;
        if journal.usn_journal_id == 0 {
            return Err(MftFallbackReason::JournalUnhealthy(0));
        }
        let (records_seen, journal_id, next_usn) = enumerate_usn_records(&handle, &journal, 64)
            .map_err(MftFallbackReason::EnumerateFailed)?;
        Ok(MftProbeOk {
            volume: volume.to_string(),
            records_seen,
            journal_id,
            next_usn,
        })
    }

    /// Attempt the MFT/USN scan for `options.roots`.
    ///
    /// Probes each distinct local volume once (UNC and non-disk roots yield
    /// [`MftFallbackReason::NonLocalVolume`] without any Win32 call).
    /// `Ok` carries one probe result per volume; `Err` carries the first
    /// failure. Entry collection itself stays on the tuned `jwalk` walk
    /// until `$MFT` FRN-to-path resolution is implemented — the probe gates
    /// [`ScanHealth`], not entry sourcing.
    pub fn mft_scan_attempt(options: &ScanOptions) -> Result<Vec<MftProbeOk>, MftFallbackReason> {
        use std::collections::BTreeSet;
        let mut volumes = BTreeSet::new();
        for root in &options.roots {
            match volume_for_root(root) {
                Some(v) => {
                    volumes.insert(v);
                }
                None => {
                    return Err(MftFallbackReason::NonLocalVolume(
                        root.to_string_lossy().into_owned(),
                    ));
                }
            }
        }
        if volumes.is_empty() && !options.roots.is_empty() {
            let first = options.roots[0].to_string_lossy().into_owned();
            return Err(MftFallbackReason::NonLocalVolume(first));
        }
        let mut out = Vec::new();
        for volume in &volumes {
            match mft_probe_volume(volume) {
                Ok(probe) => out.push(probe),
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// Derive the filesystem root (`C:\`) for a `\\.\X:` volume path.
    pub fn volume_root_path(volume: &str) -> Option<PathBuf> {
        let letter = volume.strip_prefix(r"\\.\")?.strip_suffix(':')?;
        let mut chars = letter.chars();
        let drive = chars.next()?;
        if chars.next().is_some() || !drive.is_ascii_alphabetic() {
            return None;
        }
        Some(PathBuf::from(format!(r"{}:\", drive.to_ascii_uppercase())))
    }

    /// Normalize one scan root for prefix comparison: lowercased with
    /// forward slashes folded to backslashes, so `E:/dir` roots match
    /// backslash-resolved MFT paths on case-insensitive Windows.
    pub(crate) fn normalize_root_prefix(root: &Path) -> String {
        root.to_string_lossy().to_lowercase().replace('/', "\\")
    }

    /// Case-insensitive "resolved path is under one of these roots" check.
    ///
    /// Resolved paths keep on-disk case while user roots use any case, so
    /// both sides fold before comparing. Prefixes must come from
    /// [`normalize_root_prefix`].
    fn under_prefixes(prefixes: &[String], path: &Path) -> bool {
        let folded = path.to_string_lossy().to_lowercase();
        prefixes.iter().any(|pre| {
            let pre_slash = if pre.ends_with('\\') {
                pre.clone()
            } else {
                format!("{pre}\\")
            };
            folded == *pre || folded.starts_with(&pre_slash)
        })
    }

    /// Per-chunk result of the parallel resolve/filter pass.
    struct ChunkOut {
        hits: Vec<supersearch_core::FileEntry>,
        scanned: u64,
        skipped: u64,
        errors: u64,
        orphans: u64,
    }

    /// One indexed row with MFT identity: what the delta applier needs
    /// to resolve future parent links without rescanning.
    #[derive(Debug, Clone)]
    pub struct MftIndexRow {
        /// Stat'ed entry (size/mtime real, like walk results).
        pub entry: supersearch_core::FileEntry,
        /// MFT segment index.
        pub frn: u64,
        /// Parent's MFT segment index.
        pub parent_frn: u64,
    }

    /// Outcome of [`mft_index_full`]: every resolvable record on the
    /// volume's scan roots, stat'ed, with FRN identity attached.
    #[derive(Debug)]
    pub struct MftIndexFill {
        /// All rows (unfiltered: the index stores everything the scan
        /// roots cover so later queries filter from disk, not memory).
        pub rows: Vec<MftIndexRow>,
        /// Resolved MFT paths examined.
        pub scanned: u64,
        /// Hidden-filtered records.
        pub skipped: u64,
        /// Stat failures (stale rows vanish here instead of in the index).
        pub errors: u64,
        /// Records whose parent chain could not resolve.
        pub orphans: u64,
        /// Volumes enumerated through the MFT path.
        pub volumes: Vec<String>,
        /// Per-volume phase timings (`enum`/`scan`), for the summary line.
        pub profile: String,
    }

    /// Enumerate every resolvable record under `options.roots` for index
    /// fills: like [`mft_search`] but without the name prefilter or query
    /// (the index stores everything; queries filter later). Every row
    /// carries its FRN identity so [`crate::apply_usn_deltas`] can
    /// resolve future deltas. Stat failures drop the row into `errors`.
    pub fn mft_index_full(
        options: &ScanOptions,
        progress: Option<&crate::ProgressCallback>,
    ) -> Result<MftIndexFill, MftFallbackReason> {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::Instant;
        // (rows, scanned, skipped, errors, orphans) per chunk.
        struct FillChunk {
            rows: Vec<MftIndexRow>,
            scanned: u64,
            skipped: u64,
            errors: u64,
            orphans: u64,
        }
        use std::collections::BTreeMap;
        let mut by_volume: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for root in &options.roots {
            match volume_for_root(root) {
                Some(v) => by_volume.entry(v).or_default().push(root.clone()),
                None => {
                    return Err(MftFallbackReason::NonLocalVolume(
                        root.to_string_lossy().into_owned(),
                    ));
                }
            }
        }
        let mut rows = Vec::new();
        let (mut scanned, mut skipped, mut errors, mut orphans) = (0u64, 0u64, 0u64, 0u64);
        let mut volumes = Vec::new();
        let mut profile_parts = Vec::new();
        for (volume, roots) in &by_volume {
            let anchor =
                volume_root_path(volume).ok_or_else(|| MftFallbackReason::EnumerateFailed(0))?;
            // Slash-normalized like the search path: `E:/dir` roots must
            // match backslash-resolved MFT paths.
            let prefixes: Vec<String> = roots.iter().map(|r| normalize_root_prefix(r)).collect();
            let enum_start = Instant::now();
            let names = mft_enumerate_volume(volume)?;
            let enum_ms = enum_start.elapsed().as_millis();
            let lookup = build_frn_lookup(&names);
            let scan_start = Instant::now();
            let threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .max(1);
            let chunk_len = (names.len() / (threads * 16).max(1)).clamp(25_000, 1_000_000);
            let counter = AtomicU64::new(0);
            std::thread::scope(|s| {
                let mut handles = Vec::new();
                for chunk in names.chunks(chunk_len.max(1)) {
                    let lookup = &lookup;
                    let all_names: &[MftNameEntry] = &names;
                    let anchor = &anchor;
                    let prefixes = &prefixes;
                    let counter = &counter;
                    let progress = &progress;
                    handles.push(s.spawn(move || -> FillChunk {
                        let mut rows = Vec::new();
                        let (mut scanned, mut skipped, mut errors, mut orphans) =
                            (0u64, 0u64, 0u64, 0u64);
                        for entry in chunk {
                            scanned += 1;
                            if options.skip_hidden
                                && entry.frn != ROOT_FRN
                                && (entry.is_hidden_attr() || entry.name.starts_with('.'))
                            {
                                skipped += 1;
                                continue;
                            }
                            let resolved = match resolve_one(lookup, all_names, anchor, entry) {
                                Some(r) => r,
                                None => {
                                    orphans += 1;
                                    continue;
                                }
                            };
                            if !under_prefixes(prefixes, &resolved.path) {
                                continue;
                            }
                            match crate::stat_entry(resolved.path) {
                                Some(e) => rows.push(MftIndexRow {
                                    entry: e,
                                    frn: entry.frn,
                                    parent_frn: entry.parent_frn,
                                }),
                                None => errors += 1,
                            }
                            if progress.is_some() {
                                let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
                                if n % crate::PROGRESS_STRIDE == 0 {
                                    if let Some(cb) = progress {
                                        cb(n);
                                    }
                                }
                            }
                        }
                        FillChunk {
                            rows,
                            scanned,
                            skipped,
                            errors,
                            orphans,
                        }
                    }));
                }
                for handle in handles {
                    let out = handle.join().expect("index chunk panicked");
                    rows.extend(out.rows);
                    scanned += out.scanned;
                    skipped += out.skipped;
                    errors += out.errors;
                    orphans += out.orphans;
                }
            });
            if let Some(cb) = progress {
                cb(counter.load(Ordering::Relaxed));
            }
            profile_parts.push(format!(
                "{} enum {}ms scan {}ms",
                volume,
                enum_ms,
                scan_start.elapsed().as_millis()
            ));
            volumes.push(volume.clone());
        }
        Ok(MftIndexFill {
            rows,
            scanned,
            skipped,
            errors,
            orphans,
            volumes,
            profile: profile_parts.join("; "),
        })
    }

    /// Outcome of [`mft_search`]: final fully-filtered hits.
    #[derive(Debug)]
    pub struct MftSearchOutcome {
        /// Final hits satisfying `query` (each chunk runs the full filter,
        /// so callers re-applying it is idempotent). With `want_metadata`
        /// they are stat'ed; without it they carry `size: 0, mtime: 0`.
        pub entries: Vec<supersearch_core::FileEntry>,
        /// Resolved MFT paths examined.
        pub scanned: u64,
        /// Hidden-filtered records.
        pub skipped: u64,
        /// Stat failures (stale MFT rows vanish here instead of in results)
        /// plus orphan resolutions folded in by the caller.
        pub errors: u64,
        /// Records whose parent chain could not resolve.
        pub orphans: u64,
        /// Volumes enumerated through the MFT path.
        pub volumes: Vec<String>,
        /// Per-volume phase timings (`enum`/`scan`), for the summary line.
        pub profile: String,
    }

    /// Search through the MFT name table instead of walking directories.
    ///
    /// For each distinct local volume in `options.roots`: enumerate every
    /// name record once, resolve parent chains to full paths in RAM,
    /// then keep only records under the scan roots that pass
    /// [`supersearch_query::Query::matches_name`] (fuzzy + extensions)
    /// plus the hidden filter. Survivors are stat'ed — stale rows whose
    /// files are gone fail the stat and are counted in `errors`, never
    /// returned — so results carry real size/mtime like walk results.
    ///
    /// Any non-local root, missing elevation, or Win32 failure aborts with
    /// [`MftFallbackReason`]: callers fall back to the tuned walk, whose
    /// results are identical (modulo symlinked-subtree double counting,
    /// which the flat table never produces).
    ///
    /// `want_metadata` controls stat-ing: when false (the query needs no
    /// size/date/content filters and the caller prints no metadata),
    /// entries skip the per-file stat entirely and carry `size: 0,
    /// mtime: 0` — the whole drive is then searched with zero per-file
    /// syscalls. Stale-row filtering by stat still applies when true.
    pub fn mft_search(
        options: &ScanOptions,
        query: &supersearch_query::Query,
        want_metadata: bool,
        progress: Option<&crate::ProgressCallback>,
    ) -> Result<MftSearchOutcome, MftFallbackReason> {
        use std::collections::BTreeMap;
        let mut by_volume: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for root in &options.roots {
            match volume_for_root(root) {
                Some(v) => by_volume.entry(v).or_default().push(root.clone()),
                None => {
                    return Err(MftFallbackReason::NonLocalVolume(
                        root.to_string_lossy().into_owned(),
                    ));
                }
            }
        }
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::Instant;
        let mut entries = Vec::new();
        let (mut scanned, mut skipped, mut errors, mut orphans) = (0u64, 0u64, 0u64, 0u64);
        let mut volumes = Vec::new();
        let mut profile_parts = Vec::new();
        for (volume, roots) in &by_volume {
            let anchor =
                volume_root_path(volume).ok_or_else(|| MftFallbackReason::EnumerateFailed(0))?;
            // Lowercased prefixes: resolved paths keep on-disk case while
            // user roots use any case, and Windows paths compare
            // case-insensitively. Forward slashes are normalized too, so
            // `E:/dir` roots match backslash-resolved MFT paths.
            let prefixes: Vec<String> = roots.iter().map(|r| normalize_root_prefix(r)).collect();
            let enum_start = Instant::now();
            let names = mft_enumerate_volume(volume)?;
            let enum_ms = enum_start.elapsed().as_millis();
            let lookup = build_frn_lookup(&names);
            let scan_start = Instant::now();
            // Fan resolve/filter/stat out across threads: chunks share the
            // lookup read-only, and each chunk runs the full query over
            // its staged entries (one compiled regex per chunk).
            let threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .max(1);
            let chunk_len = (names.len() / (threads * 16).max(1)).clamp(25_000, 1_000_000);
            let counter = AtomicU64::new(0);
            std::thread::scope(|s| {
                let mut handles = Vec::new();
                for chunk in names.chunks(chunk_len.max(1)) {
                    // Reborrow shared state per iteration so the `move`
                    // closure below owns only this chunk plus fresh
                    // shared references.
                    let lookup = &lookup;
                    let all_names: &[MftNameEntry] = &names;
                    let anchor = &anchor;
                    let prefixes = &prefixes;
                    let query = query;
                    let counter = &counter;
                    let progress = &progress;
                    handles.push(s.spawn(move || -> ChunkOut {
                        let mut staged = Vec::new();
                        let (mut scanned, mut skipped, mut errors, mut orphans) =
                            (0u64, 0u64, 0u64, 0u64);
                        for entry in chunk {
                            scanned += 1;
                            let is_root = entry.frn == ROOT_FRN;
                            // Hidden and the name prefilter decide on the
                            // record alone, before any path is resolved.
                            if !is_root {
                                if options.skip_hidden
                                    && (entry.is_hidden_attr() || entry.name.starts_with('.'))
                                {
                                    skipped += 1;
                                    continue;
                                }
                                if !query.matches_name(&entry.name) {
                                    continue;
                                }
                            }
                            let resolved = match resolve_one(lookup, all_names, anchor, entry) {
                                Some(r) => r,
                                None => {
                                    orphans += 1;
                                    continue;
                                }
                            };
                            if !under_prefixes(prefixes, &resolved.path) {
                                continue;
                            }
                            if !want_metadata {
                                staged.push(supersearch_core::FileEntry::new(
                                    resolved.path,
                                    0,
                                    0,
                                    resolved.is_dir,
                                ));
                                continue;
                            }
                            match crate::stat_entry(resolved.path) {
                                Some(e) => staged.push(e),
                                None => errors += 1,
                            }
                            if progress.is_some() {
                                let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
                                if n % crate::PROGRESS_STRIDE == 0 {
                                    if let Some(cb) = progress {
                                        cb(n);
                                    }
                                }
                            }
                        }
                        let hits = supersearch_query::search(&staged, query)
                            .into_iter()
                            .cloned()
                            .collect();
                        ChunkOut {
                            hits,
                            scanned,
                            skipped,
                            errors,
                            orphans,
                        }
                    }));
                }
                for handle in handles {
                    let out = handle.join().expect("scan chunk panicked");
                    entries.extend(out.hits);
                    scanned += out.scanned;
                    skipped += out.skipped;
                    errors += out.errors;
                    orphans += out.orphans;
                }
            });
            if let Some(cb) = progress {
                cb(counter.load(Ordering::Relaxed));
            }
            profile_parts.push(format!(
                "{} enum {}ms scan {}ms",
                volume,
                enum_ms,
                scan_start.elapsed().as_millis()
            ));
            volumes.push(volume.clone());
        }
        Ok(MftSearchOutcome {
            entries,
            scanned,
            skipped,
            errors,
            orphans,
            volumes,
            profile: profile_parts.join("; "),
        })
    }

    /// Outcome of [`fast_scan_or_fallback`].
    #[derive(Debug)]
    pub struct FastScanOutcome {
        /// Entries collected (always via the tuned `jwalk` walk today).
        pub result: ScanResult,
        /// Per-volume MFT probe results, or the reason the MFT path was
        /// skipped. `Ok(vec![])` means there were no roots to probe.
        pub mft_probe: Result<Vec<MftProbeOk>, MftFallbackReason>,
        /// [`ScanHealth::Healthy`] when the probe answered, else
        /// [`ScanHealth::Degraded`] with the fallback reason.
        pub health: ScanHealth,
    }

    /// Windows scan entry point: MFT attempt plus health-degraded fallback.
    ///
    /// Runs [`mft_scan_attempt`] to gate [`ScanHealth`], then collects
    /// entries with the tuned `jwalk` walk ([`crate::scan_roots`]). Works
    /// identically with and without admin — without elevation the probe
    /// reports [`MftFallbackReason::NotElevated`] and the same entries come
    /// back via `jwalk`.
    pub fn fast_scan_or_fallback(
        options: &ScanOptions,
        progress: Option<&crate::ProgressCallback>,
    ) -> FastScanOutcome {
        let probe = mft_scan_attempt(options);
        let health = match &probe {
            Ok(_) => ScanHealth::Healthy,
            Err(reason) => ScanHealth::Degraded(reason.to_string()),
        };
        let result = crate::scan_roots(options, progress);
        FastScanOutcome {
            result,
            mft_probe: probe,
            health,
        }
    }

    /// Position in the USN journal for incremental delta scans.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct UsnCursor {
        /// `UsnJournalID` seen when the cursor was recorded.
        pub journal_id: u64,
        /// Next USN to enumerate from (previous pass's journal head).
        pub next_usn: i64,
    }

    impl UsnCursor {
        /// Record a cursor from a fresh [`MftProbeOk`].
        pub fn from_probe(probe: &MftProbeOk) -> Self {
            Self {
                journal_id: probe.journal_id,
                next_usn: probe.next_usn,
            }
        }
    }

    /// Observable journal state for a delta check.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct UsnJournalState {
        /// Current `UsnJournalID`.
        pub journal_id: u64,
        /// Current journal head (`NextUsn`).
        pub next_usn: i64,
        /// Oldest still-valid USN (`LowestValidUsn`).
        pub lowest_valid_usn: i64,
    }

    /// Why a delta is impossible and a full rescan is required.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum UsnWrapReason {
        /// Journal was deleted/recreated (`UsnJournalID` changed).
        JournalIdChanged,
        /// Journal head moved backwards (wrap/overflow).
        UsnWrapped,
        /// The cursor's USN aged out past `LowestValidUsn` (truncation:
        /// records the delta would need are gone).
        JournalTruncated,
    }

    /// Incremental USN delta decision for a stored cursor.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum UsnDeltaOutcome {
        /// Journal head has not moved: nothing to do.
        NoChange,
        /// Enumerate changes from this USN forward.
        DeltaAvailable {
            /// USN to resume enumeration at (the stored cursor).
            from_usn: i64,
        },
        /// Journal wrapped or was recreated: deltas are unsound, the
        /// caller must run a full rescan (see [`rescan_on_wrap`]).
        RescanRequired {
            /// What invalidated the cursor.
            reason: UsnWrapReason,
        },
    }

    /// Decide between an incremental USN delta and a full rescan.
    ///
    /// Stub policy (no I/O): pure function of the stored cursor and the
    /// current journal state. Any wrap — journal recreation, a backwards
    /// head, or cursor truncation — maps to
    /// [`UsnDeltaOutcome::RescanRequired`].
    pub fn usn_delta_since(cursor: UsnCursor, current: UsnJournalState) -> UsnDeltaOutcome {
        if current.journal_id != cursor.journal_id {
            return UsnDeltaOutcome::RescanRequired {
                reason: UsnWrapReason::JournalIdChanged,
            };
        }
        if current.next_usn < cursor.next_usn {
            return UsnDeltaOutcome::RescanRequired {
                reason: UsnWrapReason::UsnWrapped,
            };
        }
        if cursor.next_usn < current.lowest_valid_usn {
            return UsnDeltaOutcome::RescanRequired {
                reason: UsnWrapReason::JournalTruncated,
            };
        }
        if current.next_usn == cursor.next_usn {
            UsnDeltaOutcome::NoChange
        } else {
            UsnDeltaOutcome::DeltaAvailable {
                from_usn: cursor.next_usn,
            }
        }
    }

    /// Serve a [`UsnDeltaOutcome::RescanRequired`] with a full `jwalk`
    /// rescan. Returns the fresh [`ScanResult`].
    pub fn rescan_on_wrap(
        options: &ScanOptions,
        progress: Option<&crate::ProgressCallback>,
    ) -> ScanResult {
        crate::scan_roots(options, progress)
    }

    /// Delta transport: `FSCTL_ENUM_USN_DATA` over a USN range serves as
    /// the journal delta reader. `FSCTL_READ_USN_JOURNAL` (`0x900FB`,
    /// function 62/method-neither) is NOT served by this driver:
    /// neighbouring codes (`QUERY` `0x900F4`, `ENUM` `0x900B3`,
    /// `CREATE` `0x900E7`, `DELETE` `0x900F8`) all answer while every
    /// `READ_USN_JOURNAL_DATA_V0` (40 B) and `_V1` (44 B) shape fails
    /// with `ERROR_INVALID_FUNCTION`, a result reproduced from raw
    /// probes outside this codebase while `fsutil usn readjournal`
    /// (ENUM-based) reads fine. The range form
    /// (`LowUsn`/`HighUsn` in [`MftEnumData`]) yields the same
    /// `USN_RECORD` stream, paged by FRN with the usual 8-byte
    /// next-FRN header, so one code path serves full scans and ticks.

    /// One journal change: identity, membership decision inputs, and
    /// position for cursor advancement.
    #[derive(Debug, Clone)]
    pub struct UsnDelta {
        /// MFT segment index of the changed file.
        pub frn: u64,
        /// MFT segment index of its parent directory.
        pub parent_frn: u64,
        /// File name at change time.
        pub name: String,
        /// `FileAttributes` from the record.
        pub attributes: u32,
        /// This record's USN (monotonic per journal).
        pub usn: i64,
        /// This record's reason mask (see [`classify_delta`]).
        pub reason: u32,
    }

    impl UsnDelta {
        /// Directory bit, mirroring [`MftNameEntry::is_dir`].
        pub fn is_dir(&self) -> bool {
            self.attributes & ATTR_DIRECTORY != 0
        }
        /// Membership decision for this record.
        pub fn kind(&self) -> DeltaKind {
            classify_delta(self.reason)
        }
    }

    /// Outcome of [`read_usn_deltas`]: records since the cursor, or the
    /// reason a full rescan is required instead.
    #[derive(Debug)]
    pub enum DeltaReadOutcome {
        /// Journal head has not moved: index is current.
        NoChange,
        /// Journal records to apply, in USN order.
        Deltas {
            /// Records since the cursor (possibly empty on a quiet
            /// volume that still advanced its head).
            deltas: Vec<UsnDelta>,
            /// Cursor to store when the batch is applied: the maximum
            /// USN observed, or the read start when nothing arrived.
            next_usn: i64,
        },
        /// Journal wrapped, truncated, or recreated: deltas are unsound,
        /// the caller must run a full rescan (see [`rescan_on_wrap`]).
        RescanRequired {
            /// What invalidated the cursor.
            reason: UsnWrapReason,
        },
    }

    /// Read journal changes since `cursor` on `volume` (`\\.\X:`).
    ///
    /// Opens the volume (elevation-gated), checks the stored cursor
    /// against current journal state via [`usn_delta_since`], then pages
    /// `FSCTL_ENUM_USN_DATA` over `[from_usn, +inf)` in 64 KiB batches
    /// (FRN-paged, 8-byte next-FRN header per batch, same records the
    /// full scan walks). A batch cap with FRNs still pending banks what
    /// was collected WITHOUT advancing the cursor, so the next tick
    /// resumes the same range; only an FRN-exhausted tick advances to
    /// the maximum USN seen. Any wrap, truncation, recreation, or Win32
    /// failure maps to [`MftFallbackReason`] (full rescan) or
    /// [`DeltaReadOutcome::RescanRequired`], never to partial deltas.
    pub fn read_usn_deltas(
        volume: &str,
        cursor: UsnCursor,
        max_batches: u32,
    ) -> Result<DeltaReadOutcome, MftFallbackReason> {
        if !is_elevated() {
            return Err(MftFallbackReason::NotElevated);
        }
        let handle = open_volume(volume).map_err(|e| {
            if e == ERROR_ACCESS_DENIED {
                MftFallbackReason::AccessDenied
            } else {
                MftFallbackReason::EnumerateFailed(e)
            }
        })?;
        let journal = query_journal(&handle).map_err(|_| MftFallbackReason::JournalUnhealthy(0))?;
        if journal.usn_journal_id == 0 {
            return Err(MftFallbackReason::JournalUnhealthy(0));
        }
        let state = UsnJournalState {
            journal_id: journal.usn_journal_id,
            next_usn: journal.next_usn,
            lowest_valid_usn: journal.lowest_valid_usn,
        };
        let from_usn = match usn_delta_since(cursor, state) {
            UsnDeltaOutcome::NoChange => return Ok(DeltaReadOutcome::NoChange),
            UsnDeltaOutcome::RescanRequired { reason } => {
                return Ok(DeltaReadOutcome::RescanRequired { reason });
            }
            UsnDeltaOutcome::DeltaAvailable { from_usn } => from_usn,
        };
        let mut out = vec![0u8; 64 * 1024];
        let mut deltas = Vec::new();
        let mut max_usn = from_usn;
        let mut start_frn = 0u64;
        let mut finished = false;
        for _ in 0..max_batches.max(1) {
            let input = MftEnumData {
                start_file_reference_number: start_frn,
                low_usn: from_usn,
                high_usn: i64::MAX,
            };
            let mut returned = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    handle.0,
                    FSCTL_ENUM_USN_DATA,
                    (&input as *const MftEnumData).cast(),
                    std::mem::size_of::<MftEnumData>() as u32,
                    out.as_mut_ptr().cast(),
                    out.len() as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let err = unsafe { GetLastError() };
                if err == ERROR_HANDLE_EOF {
                    finished = true;
                    break;
                }
                return Err(MftFallbackReason::EnumerateFailed(err));
            }
            if returned <= 8 {
                finished = true;
                break;
            }
            // Bytes [0..8) are the next-enumeration FRN; records follow.
            let end = (returned as usize).min(out.len());
            let next_frn = u64::from_ne_bytes(out[0..8].try_into().unwrap_or([0; 8]));
            let mut off = 8usize;
            while off + 4 <= end {
                match parse_usn_record_full(&out[off..end]) {
                    Some((entry, usn, reason, len)) => {
                        max_usn = max_usn.max(usn);
                        deltas.push(UsnDelta {
                            frn: entry.frn,
                            parent_frn: entry.parent_frn,
                            name: entry.name,
                            attributes: entry.attributes,
                            usn,
                            reason,
                        });
                        off += len;
                    }
                    None => {
                        let len = u32::from_le_bytes(out[off..off + 4].try_into().unwrap_or([0; 4]))
                            as usize;
                        if len < 4 || off + len > end {
                            break;
                        }
                        off += len;
                    }
                }
            }
            if next_frn == 0 || next_frn == start_frn {
                finished = true;
                break;
            }
            start_frn = next_frn;
        }
        // Cap exhausted with FRNs still pending: bank what was collected
        // WITHOUT advancing the cursor. ENUM pages in FRN order, so every
        // unseen record still sits above the stored cursor and is re-read
        // by the next tick (applier upserts are idempotent).
        let next_usn = if finished { max_usn } else { from_usn };
        Ok(DeltaReadOutcome::Deltas { deltas, next_usn })
    }

    /// Build the current [`UsnJournalState`] for `volume` (`\\.\X:`), or
    /// `None` when the journal cannot be queried (not elevated, unhealthy,
    /// non-NTFS). Used to feed [`usn_delta_since`].
    pub fn current_journal_state(volume: &str) -> Option<UsnJournalState> {
        let handle = open_volume(volume).ok()?;
        let journal = query_journal(&handle).ok()?;
        if journal.usn_journal_id == 0 {
            return None;
        }
        Some(UsnJournalState {
            journal_id: journal.usn_journal_id,
            next_usn: journal.next_usn,
            lowest_valid_usn: journal.lowest_valid_usn,
        })
    }

    /// Drive-letter roots of `options` that have no local volume (UNC, ...).
    /// Non-empty means the MFT path can never serve this scan.
    pub fn non_local_roots(options: &ScanOptions) -> Vec<PathBuf> {
        options
            .roots
            .iter()
            .filter(|r| volume_for_root(r).is_none())
            .cloned()
            .collect()
    }

    /// Live delta round-trip through the ENUM-range transport: elevated
    /// only, skipped elsewhere so unelevated CI stays green. Pins the
    /// semantics the applier depends on: a live file whose USN advanced
    /// since the cursor arrives once as an upsert (current name, parent,
    /// FRN); a deleted file yields NO tombstone record (ENUM walks
    /// per-file current state, so deletes converge through full-sync
    /// tombstoning, never through deltas).
    #[test]
    fn live_enum_range_reports_create_upsert_and_no_delete_tombstone() {
        if !is_elevated() {
            return;
        }
        let volume = r"\\.\C:";
        let dir = std::env::temp_dir().join("ss-journal-probe");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("probe-roundtrip.txt");
        let _ = std::fs::remove_file(&target);
        let state = current_journal_state(volume).expect("journal state");
        let mut cursor = UsnCursor {
            journal_id: state.journal_id,
            next_usn: state.next_usn,
        };
        std::fs::write(&target, b"v1").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let mut saw_create = false;
        let mut create_frn = 0u64;
        match read_usn_deltas(volume, cursor, 64).expect("delta read after create") {
            DeltaReadOutcome::NoChange => panic!("create must advance the journal head"),
            DeltaReadOutcome::Deltas { deltas, next_usn } => {
                assert!(next_usn >= cursor.next_usn, "cursor must not go backwards");
                cursor.next_usn = next_usn;
                for d in &deltas {
                    if d.name == "probe-roundtrip.txt" {
                        assert_eq!(d.kind(), DeltaKind::Upsert);
                        assert_ne!(d.frn, 0);
                        saw_create = true;
                        create_frn = d.frn;
                    }
                }
            }
            DeltaReadOutcome::RescanRequired { .. } => panic!("fresh cursor wrapped?!"),
        }
        assert!(saw_create, "no create/upsert record observed");
        assert_ne!(create_frn, 0);
        // Delete it: the range walk must NOT report the file again (no
        // tombstone), while the cursor still advances past the delete.
        std::fs::remove_file(&target).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        match read_usn_deltas(volume, cursor, 64).expect("delta read after delete") {
            DeltaReadOutcome::NoChange => {}
            DeltaReadOutcome::Deltas { deltas, next_usn } => {
                assert!(next_usn >= cursor.next_usn, "cursor must not go backwards");
                for d in &deltas {
                    assert_ne!(
                        d.name, "probe-roundtrip.txt",
                        "deleted file must not reappear as a delta",
                    );
                }
            }
            DeltaReadOutcome::RescanRequired { .. } => panic!("cursor wrapped?!"),
        }
        let _ = std::fs::remove_dir(&dir);
    }

    /// Live journal read at the current head: elevated only, skipped
    /// elsewhere so unelevated CI stays green.
    #[test]
    fn live_delta_read_at_head_is_quiet() {
        if !is_elevated() {
            return;
        }
        let volume = r"\\.\C:";
        let state = current_journal_state(volume).expect("journal state");
        let cursor = UsnCursor {
            journal_id: state.journal_id,
            next_usn: state.next_usn,
        };
        match read_usn_deltas(volume, cursor, 4).expect("delta read") {
            DeltaReadOutcome::NoChange => {}
            // Background writes may land between the two calls; a small
            // batch is still a healthy read.
            DeltaReadOutcome::Deltas { deltas, next_usn } => {
                assert!(deltas.len() < 10_000, "suspicious burst");
                assert!(next_usn >= cursor.next_usn);
            }
            DeltaReadOutcome::RescanRequired { .. } => {
                panic!("fresh cursor must not require rescan")
            }
        }
    }

    #[cfg(test)]
    mod record_tests {
        use super::*;

        /// Build a synthetic `USN_RECORD_V2` for `name` (UTF-16 encoded).
        fn v2_record(frn: u64, parent: u64, attrs: u32, name: &str) -> Vec<u8> {
            let units: Vec<u16> = name.encode_utf16().collect();
            let name_len = units.len() * 2;
            let len = 60 + name_len;
            let mut buf = vec![0u8; len];
            buf[0..4].copy_from_slice(&(len as u32).to_le_bytes());
            buf[4..6].copy_from_slice(&2u16.to_le_bytes());
            buf[8..16].copy_from_slice(&frn.to_le_bytes());
            buf[16..24].copy_from_slice(&parent.to_le_bytes());
            buf[52..56].copy_from_slice(&attrs.to_le_bytes());
            buf[56..58].copy_from_slice(&(name_len as u16).to_le_bytes());
            buf[58..60].copy_from_slice(&60u16.to_le_bytes());
            for (i, unit) in units.iter().enumerate() {
                buf[60 + i * 2..62 + i * 2].copy_from_slice(&unit.to_le_bytes());
            }
            buf
        }

        #[test]
        fn v2_records_parse_with_names_and_attrs() {
            // Sequence numbers live above bit 48: masking must recover
            // the segment index underneath them.
            let mut stream = v2_record((1u64 << 48) | 5, 5, 0x10, "docs");
            stream.extend_from_slice(&v2_record(
                (2u64 << 48) | 7,
                (1u64 << 48) | 5,
                0x20,
                "notes.txt",
            ));
            let (first, stride) = parse_usn_record(&stream).expect("first record");
            assert_eq!(first.frn, 5);
            assert_eq!(first.parent_frn, 5);
            assert_eq!(first.name, "docs");
            assert!(first.is_dir());
            // Sequence bits are masked: full FRN differs, segment matches.
            let (second, _) = parse_usn_record(&stream[stride..]).expect("second record");
            assert_eq!(second.frn, 7);
            assert_eq!(second.parent_frn, 5);
            assert_eq!(second.name, "notes.txt");
            assert!(!second.is_dir());
        }

        #[test]
        fn batch_parse_skips_header_and_walks_records() {
            // A raw enumeration batch: 8-byte next-FRN header followed by
            // packed records, exactly what the ioctl hands back.
            let mut batch = 12345u64.to_le_bytes().to_vec();
            batch.extend_from_slice(&v2_record(6, 5, 0x20, "a.txt"));
            batch.extend_from_slice(&v2_record(7, 5, 0x10, "sub"));
            let len = batch.len();
            let (entries, skipped, _) = parse_batch(batch, len);
            assert_eq!(skipped, 0);
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].name, "a.txt");
            assert!(entries[1].is_dir());
        }

        /// Build a synthetic `USN_RECORD_V2` carrying journal fields.
        fn v2_delta_record(
            frn: u64,
            parent: u64,
            attrs: u32,
            usn: i64,
            reason: u32,
            name: &str,
        ) -> Vec<u8> {
            let mut rec = v2_record(frn, parent, attrs, name);
            rec[24..32].copy_from_slice(&usn.to_le_bytes());
            rec[40..44].copy_from_slice(&reason.to_le_bytes());
            rec
        }

        /// Parse an ENUM-shaped delta batch the way [`read_usn_deltas`]
        /// walks `FSCTL_ENUM_USN_DATA` output: 8-byte next-FRN header,
        /// then `USN_RECORD`s.
        fn parse_delta_stream(buf: &[u8]) -> Vec<UsnDelta> {
            let mut out = Vec::new();
            let mut off = 8usize;
            while off + 4 <= buf.len() {
                match parse_usn_record_full(&buf[off..]) {
                    Some((entry, usn, reason, len)) => {
                        out.push(UsnDelta {
                            frn: entry.frn,
                            parent_frn: entry.parent_frn,
                            name: entry.name,
                            attributes: entry.attributes,
                            usn,
                            reason,
                        });
                        off += len;
                    }
                    None => break,
                }
            }
            out
        }

        #[test]
        fn delta_stream_carries_usn_reason_and_kind() {
            let mut stream = Vec::new();
            // ENUM-shaped batch: 8-byte next-FRN header first.
            stream.extend_from_slice(&0u64.to_ne_bytes());
            stream.extend_from_slice(&v2_delta_record(10, 5, 0x20, 101, 0x100, "new.txt"));
            stream.extend_from_slice(&v2_delta_record(11, 5, 0x20, 102, 0x200, "gone.txt"));
            stream.extend_from_slice(&v2_delta_record(12, 5, 0x20, 103, 0x1000, "old.txt"));
            stream.extend_from_slice(&v2_delta_record(13, 5, 0x20, 104, 0x2000, "new-name.txt"));
            stream.extend_from_slice(&v2_delta_record(14, 5, 0x20, 105, 0x1, "touched.txt"));
            let deltas = parse_delta_stream(&stream);
            assert_eq!(deltas.len(), 5);
            assert_eq!(deltas[0].usn, 101);
            assert_eq!(deltas[0].kind(), DeltaKind::Upsert);
            assert_eq!(deltas[1].kind(), DeltaKind::Remove);
            assert_eq!(deltas[2].kind(), DeltaKind::Remove);
            assert_eq!(deltas[3].kind(), DeltaKind::Upsert);
            assert_eq!(deltas[4].kind(), DeltaKind::Upsert);
            assert_eq!(deltas[4].name, "touched.txt");
        }

        #[test]
        fn garbage_buffers_do_not_parse() {
            assert!(parse_usn_record(&[]).is_none());
            assert!(parse_usn_record(&[0u8; 59]).is_none());
            // Declared length beyond the buffer.
            let mut bad = v2_record(6, 5, 0, "x");
            bad[0..4].copy_from_slice(&9999u32.to_le_bytes());
            assert!(parse_usn_record(&bad).is_none());
            // Unknown major version.
            let mut v9 = v2_record(6, 5, 0, "x");
            v9[4..6].copy_from_slice(&9u16.to_le_bytes());
            assert!(parse_usn_record(&v9).is_none());
        }

        #[test]
        fn paths_resolve_through_parent_chains() {
            let entries = vec![
                MftNameEntry {
                    frn: 5,
                    parent_frn: 5,
                    name: ".".into(),
                    attributes: 0x10,
                },
                MftNameEntry {
                    frn: 10,
                    parent_frn: 5,
                    name: "docs".into(),
                    attributes: 0x10,
                },
                MftNameEntry {
                    frn: 11,
                    parent_frn: 10,
                    name: "notes.txt".into(),
                    attributes: 0x20,
                },
                MftNameEntry {
                    frn: 12,
                    parent_frn: 999,
                    name: "orphan.txt".into(),
                    attributes: 0x20,
                },
            ];
            let (resolved, orphans) = resolve_mft_paths(Path::new(r"C:\"), &entries);
            assert_eq!(orphans, 1);
            let mut paths: Vec<_> = resolved.iter().map(|r| r.path.clone()).collect();
            paths.sort();
            assert_eq!(
                paths,
                vec![
                    PathBuf::from(r"C:\"),
                    PathBuf::from(r"C:\docs"),
                    PathBuf::from(r"C:\docs\notes.txt"),
                ]
            );
            assert!(
                resolved
                    .iter()
                    .find(|r| r.path.ends_with("docs"))
                    .unwrap()
                    .is_dir
            );
        }

        #[test]
        fn volume_roots_derive_from_device_paths() {
            assert_eq!(volume_root_path(r"\\.\C:"), Some(PathBuf::from(r"C:\")));
            assert_eq!(volume_root_path(r"\\server\share"), None);
            assert_eq!(volume_root_path("relative"), None);
        }
    }
}

/// Linux fast path: the tuned `jwalk` walk plus `inotify` watch-limit
/// handling with poll fallback.
///
/// `notify`'s inotify backend is bounded by
/// `/proc/sys/fs/inotify/max_user_watches`; when recursive watches exhaust
/// it, registration fails with [`notify::ErrorKind::MaxFilesWatch`] (or
/// `ENOSPC`) and [`start_watch_or_poll`](crate::platform::start_watch_or_poll)
/// keeps the index live via [`crate::spawn_poll_thread`].
#[cfg(target_os = "linux")]
pub mod linux {
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::ScanOptions;

    use super::{is_watch_limit_error, WatchOrPoll};

    /// Rayon directory-reader parallelism for the tuned `jwalk` walk.
    ///
    /// Matches [`crate::DEFAULT_PARALLELISM`] on Linux; centralized here so
    /// the Linux fast path and the generic walk cannot drift apart.
    pub fn tuned_parallelism() -> usize {
        crate::DEFAULT_PARALLELISM
    }

    /// Read `/proc/sys/fs/inotify/max_user_watches`.
    ///
    /// Returns `None` when the file cannot be read or parsed (non-Linux
    /// kernels, containers hiding procfs, permission errors). Used only for
    /// diagnostics: the authoritative signal is still the registration
    /// error matched by [`is_watch_limit_error`](super::is_watch_limit_error).
    pub fn read_max_user_watches() -> Option<u32> {
        let raw = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches").ok()?;
        raw.trim().parse::<u32>().ok()
    }

    /// Outcome of [`start_watch_or_poll`]: re-exported here so Linux
    /// callers do not need to import the parent module.
    pub use super::WatchOrPoll as LinuxWatchOrPoll;

    /// Watch `options.roots` with `inotify`, falling back to interval
    /// polling when `max_user_watches` is exhausted.
    ///
    /// Thin wrapper over
    /// [`start_watch_or_poll`](super::start_watch_or_poll): tries
    /// [`WatcherStub::start_with_db`], and on any registration error —
    /// watch-limit or otherwise — keeps the index fresh with a
    /// [`PollHandle`] ticking every `poll_interval`.
    pub fn start_watch_or_poll(
        options: &ScanOptions,
        db_path: PathBuf,
        poll_interval: Duration,
    ) -> WatchOrPoll {
        super::start_watch_or_poll(options, db_path, poll_interval)
    }

    /// True when `err` is an `inotify` watch-limit exhaustion.
    ///
    /// Alias of [`is_watch_limit_error`](super::is_watch_limit_error) for
    /// Linux callers.
    pub fn is_watch_limit(err: &notify::Error) -> bool {
        is_watch_limit_error(err)
    }
}

#[cfg(test)]
mod platform_tests {
    use super::*;

    #[test]
    fn unc_paths_detected_slash_spellings_kept() {
        assert!(is_unc_path(Path::new(r"\\server\share\dir")));
        assert!(is_unc_path(Path::new("//server/share/dir")));
        assert!(is_unc_path(Path::new(r"\\?\UNC\server\share")));
        assert!(!is_unc_path(Path::new(r"C:\dir\file.txt")));
        assert!(!is_unc_path(Path::new(r"\\?\C:\dir")));
        assert!(!is_unc_path(Path::new("/tmp/root")));
        assert!(!is_unc_path(Path::new("relative/root")));
    }

    /// Forward-slash roots must normalize to the backslash spelling, or
    /// MFT prefix filtering silently matches nothing (regression: a
    /// `--root E:/dir` search scanned the whole volume with zero hits).
    #[test]
    #[cfg(windows)]
    fn root_prefixes_normalize_slashes_and_case() {
        use super::windows::normalize_root_prefix;
        assert_eq!(
            normalize_root_prefix(Path::new("E:/AI/Code")),
            r"e:\ai\code"
        );
        assert_eq!(
            normalize_root_prefix(Path::new(r"E:\AI\Code")),
            r"e:\ai\code"
        );
        assert_eq!(
            normalize_root_prefix(Path::new("e:/ai/code/")),
            r"e:\ai\code\"
        );
    }

    #[test]
    fn verbatim_prefix_stripped_for_display_only() {
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\C:\a\b")),
            PathBuf::from(r"C:\a\b")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\UNC\server\share\x")),
            PathBuf::from(r"\\server\share\x")
        );
        // Non-verbatim paths pass through untouched.
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"C:\a")),
            PathBuf::from(r"C:\a")
        );
    }

    #[test]
    fn watch_limit_errors_detected() {
        let max = notify::Error::new(notify::ErrorKind::MaxFilesWatch);
        assert!(is_watch_limit_error(&max));
        let enospc = notify::Error::io(std::io::Error::from_raw_os_error(28));
        assert!(is_watch_limit_error(&enospc));
        let other = notify::Error::new(notify::ErrorKind::PathNotFound);
        assert!(!is_watch_limit_error(&other));
    }

    #[test]
    fn watch_or_poll_falls_back_when_registration_fails() {
        // A watch root that can never exist forces registration to fail,
        // so the poll fallback must engage.
        let tmp = tempfile::tempdir().unwrap();
        let missing_root: PathBuf = std::env::temp_dir().join(format!(
            "supersearch-no-such-root-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        assert!(!missing_root.exists());
        let opts = ScanOptions::new(vec![missing_root]);
        let feed = start_watch_or_poll(
            &opts,
            tmp.path().join("index.db"),
            Duration::from_millis(50),
        );
        assert!(
            feed.is_poll_fallback(),
            "failed registration must engage the poll fallback"
        );
        feed.stop_and_join();
    }

    #[cfg(windows)]
    mod windows_tests {
        use super::super::windows::*;
        use crate::ScanOptions;

        #[test]
        fn elevation_probe_runs_without_admin() {
            // Must not panic or require privileges; the value depends on
            // the environment (false in CI without elevation).
            let _ = is_elevated();
        }

        #[test]
        fn unc_roots_never_touch_the_mft_path() {
            let opts = ScanOptions::new(vec![std::path::PathBuf::from(r"\\server\share")]);
            assert_eq!(
                mft_scan_attempt(&opts),
                Err(MftFallbackReason::NonLocalVolume(
                    r"\\server\share".to_string()
                ))
            );
            assert_eq!(non_local_roots(&opts).len(), 1);
            assert!(volume_for_root(std::path::Path::new(r"\\server\share")).is_none());
        }

        #[test]
        fn drive_roots_map_to_volume_handles() {
            assert_eq!(
                volume_for_root(std::path::Path::new(r"C:\dir")),
                Some(r"\\.\C:".to_string())
            );
            assert_eq!(
                volume_for_root(std::path::Path::new(r"d:\x")),
                Some(r"\\.\D:".to_string())
            );
        }

        #[test]
        fn fast_scan_matches_jwalk_with_or_without_elevation() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("root");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("a.txt"), b"hello").unwrap();
            let opts = ScanOptions::new(vec![root.clone()]);
            let outcome = fast_scan_or_fallback(&opts, None);
            let baseline = crate::scan_roots(&opts, None);
            let mut got: Vec<_> = outcome.result.entries.iter().map(|e| &e.path).collect();
            let mut want: Vec<_> = baseline.entries.iter().map(|e| &e.path).collect();
            got.sort();
            want.sort();
            assert_eq!(got, want);
            // Health reflects the probe; entries are identical either way.
            match &outcome.mft_probe {
                Ok(_) => assert_eq!(outcome.health, ScanHealth::Healthy),
                Err(reason) => assert_eq!(outcome.health, ScanHealth::Degraded(reason.to_string())),
            }
        }

        #[test]
        fn usn_wrap_cases_require_rescan() {
            let cursor = UsnCursor {
                journal_id: 7,
                next_usn: 100,
            };
            // Steady state.
            assert_eq!(
                usn_delta_since(
                    cursor,
                    UsnJournalState {
                        journal_id: 7,
                        next_usn: 100,
                        lowest_valid_usn: 0
                    }
                ),
                UsnDeltaOutcome::NoChange
            );
            // Forward progress: incremental delta.
            assert_eq!(
                usn_delta_since(
                    cursor,
                    UsnJournalState {
                        journal_id: 7,
                        next_usn: 150,
                        lowest_valid_usn: 0
                    }
                ),
                UsnDeltaOutcome::DeltaAvailable { from_usn: 100 }
            );
            // Journal recreated.
            assert_eq!(
                usn_delta_since(
                    cursor,
                    UsnJournalState {
                        journal_id: 8,
                        next_usn: 150,
                        lowest_valid_usn: 0
                    }
                ),
                UsnDeltaOutcome::RescanRequired {
                    reason: UsnWrapReason::JournalIdChanged
                }
            );
            // Head moved backwards: wrap.
            assert_eq!(
                usn_delta_since(
                    cursor,
                    UsnJournalState {
                        journal_id: 7,
                        next_usn: 50,
                        lowest_valid_usn: 0
                    }
                ),
                UsnDeltaOutcome::RescanRequired {
                    reason: UsnWrapReason::UsnWrapped
                }
            );
            // Cursor aged out past LowestValidUsn: truncation.
            assert_eq!(
                usn_delta_since(
                    cursor,
                    UsnJournalState {
                        journal_id: 7,
                        next_usn: 200,
                        lowest_valid_usn: 120
                    }
                ),
                UsnDeltaOutcome::RescanRequired {
                    reason: UsnWrapReason::JournalTruncated
                }
            );
        }

        #[test]
        fn rescan_on_wrap_recovers_entries() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("root");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("w.txt"), b"w").unwrap();
            let opts = ScanOptions::new(vec![root.clone()]);
            let result = rescan_on_wrap(&opts, None);
            assert!(result.entries.iter().any(|e| e.path == root.join("w.txt")));
        }
    }

    #[cfg(target_os = "linux")]
    mod linux_tests {
        use super::super::linux::*;

        #[test]
        fn tuned_parallelism_tracks_default() {
            assert_eq!(tuned_parallelism(), crate::DEFAULT_PARALLELISM);
        }

        #[test]
        fn max_user_watches_read_is_optional() {
            // Must not panic; None is legitimate in containers.
            let _ = read_max_user_watches();
        }
    }
}

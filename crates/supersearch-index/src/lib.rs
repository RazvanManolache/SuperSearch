use rusqlite::{Connection, Result};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use supersearch_core::FileEntry;

/// Platform fast paths (Windows MFT/USN attempt with `jwalk` fallback,
/// Linux `inotify` watch-limit handling with poll fallback, UNC/long-path
/// helpers).
pub mod platform;

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    is_dir INTEGER NOT NULL,
    frn INTEGER,
    parent_frn INTEGER
) STRICT;
CREATE TABLE IF NOT EXISTS usn_cursor (
    volume TEXT PRIMARY KEY,
    journal_id INTEGER NOT NULL,
    next_usn INTEGER NOT NULL,
    updated INTEGER NOT NULL
) STRICT;";

/// Column names added to `files` after the first schema revision.
/// Databases created by older releases gain them through [`init_db`].
const FILES_ADDED_COLUMNS: &[(&str, &str)] = &[("frn", "INTEGER"), ("parent_frn", "INTEGER")];

/// Open (or create) the index database and ensure schema exists.
///
/// Besides creating fresh tables, migrates databases written by older
/// releases: any missing `files` column from [`FILES_ADDED_COLUMNS`] is
/// added with `ALTER TABLE`, preserving existing rows (new columns read
/// back as `NULL`, i.e. "FRN unknown").
pub fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    let mut stmt = conn.prepare("PRAGMA table_info(files)")?;
    let present: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    for (name, ty) in FILES_ADDED_COLUMNS {
        if !present.iter().any(|c| c == name) {
            conn.execute_batch(&format!("ALTER TABLE files ADD COLUMN {name} {ty}"))?;
        }
    }
    // After the column migration above: `frn` is guaranteed present,
    // so the index builds on fresh and legacy databases alike. It must
    // not live in `SCHEMA`: legacy tables gain `frn` only through the
    // migration, and indexing a missing column aborts the batch.
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_files_frn ON files(frn)")?;
    Ok(())
}

/// Upsert one entry into the `files` table, without FRN identity
/// (walk-sourced rows).
pub fn upsert(conn: &Connection, e: &supersearch_core::FileEntry) -> Result<()> {
    upsert_full(conn, e, None, None)
}

/// Upsert one entry into the `files` table with optional MFT identity.
///
/// `frn`/`parent_frn` are the masked segment indexes from the name
/// table; walk-sourced rows pass `None` ("FRN unknown"). Re-upserts
/// overwrite: a later walk without FRNs clears stale FRN columns rather
/// than leaving a wrong identity behind.
pub fn upsert_full(
    conn: &Connection,
    e: &supersearch_core::FileEntry,
    frn: Option<u64>,
    parent_frn: Option<u64>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO files (path, size, mtime, is_dir, frn, parent_frn) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(path) DO UPDATE SET size=excluded.size, mtime=excluded.mtime, is_dir=excluded.is_dir, frn=excluded.frn, parent_frn=excluded.parent_frn",
        rusqlite::params![
            e.path.to_string_lossy().as_ref(),
            e.size as i64,
            e.mtime,
            i64::from(e.is_dir),
            frn.map(|f| f as i64),
            parent_frn.map(|f| f as i64),
        ],
    )?;
    Ok(())
}

/// A stored USN journal cursor: where one volume's incremental delta
/// reads resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredUsnCursor {
    /// `UsnJournalID` seen when the cursor was recorded.
    pub journal_id: u64,
    /// Next USN to enumerate from.
    pub next_usn: i64,
    /// Unix seconds when the cursor was stored.
    pub updated: i64,
}

/// Persist one volume's USN cursor (insert or replace).
pub fn save_usn_cursor(
    conn: &Connection,
    volume: &str,
    journal_id: u64,
    next_usn: i64,
) -> Result<StoredUsnCursor> {
    let updated = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    conn.execute(
        "INSERT INTO usn_cursor (volume, journal_id, next_usn, updated) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(volume) DO UPDATE SET journal_id=excluded.journal_id, next_usn=excluded.next_usn, updated=excluded.updated",
        rusqlite::params![volume, journal_id as i64, next_usn, updated],
    )?;
    Ok(StoredUsnCursor {
        journal_id,
        next_usn,
        updated,
    })
}

/// Load one volume's USN cursor, or `None` when the volume was never
/// indexed incrementally (caller runs a full scan first).
pub fn load_usn_cursor(conn: &Connection, volume: &str) -> Result<Option<StoredUsnCursor>> {
    let mut stmt =
        conn.prepare("SELECT journal_id, next_usn, updated FROM usn_cursor WHERE volume = ?1")?;
    let mut rows = stmt.query_map([volume], |row| {
        Ok(StoredUsnCursor {
            journal_id: row.get::<_, i64>(0)? as u64,
            next_usn: row.get(1)?,
            updated: row.get(2)?,
        })
    })?;
    match rows.next() {
        Some(row) => row.map(Some),
        None => Ok(None),
    }
}

/// Apply one batch of USN journal deltas to the index (Windows only).
///
/// Resolution combines indexed FRN rows with the batch itself (first
/// record per FRN wins, so rename-old resolves the old path even when
/// the rename-new record for the same file follows in the same batch).
/// Removes apply unconditionally; upserts respect `skip_hidden`. A
/// stat failure on an upsert converges to a removal: the file vanished
/// between the journal record and the apply.
#[cfg(windows)]
#[derive(Debug, Default)]
pub struct DeltaApplyStats {
    /// Rows upserted (creates, writes, rename-new sides).
    pub upserted: u64,
    /// Rows removed (deletes, rename-old sides, stat-failed upserts).
    pub removed: u64,
    /// Deltas that could not resolve or were hidden-filtered.
    pub skipped: u64,
    /// Unexpected failures (reserved; stat races count as removals).
    pub errors: u64,
}

/// Fold [`platform::windows::UsnDelta`] records into `conn`.
///
/// `volume_root` anchors FRN resolution (e.g. `C:\`). Returns per-batch
/// counters; never fails the whole batch on one bad record.
#[cfg(windows)]
pub fn apply_usn_deltas(
    conn: &Connection,
    volume_root: &Path,
    skip_hidden: bool,
    deltas: &[platform::windows::UsnDelta],
) -> Result<DeltaApplyStats> {
    use platform::windows::MftNameEntry;
    use std::collections::HashMap;

    // Indexed identity first, then the batch overlaid first-wins per
    // FRN (see the rename-old/new ordering note above).
    let mut entries: Vec<MftNameEntry> = Vec::new();
    let mut seen: HashMap<u64, ()> = HashMap::new();
    {
        let mut stmt =
            conn.prepare("SELECT path, is_dir, frn, parent_frn FROM files WHERE frn IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            let path: String = row.get(0)?;
            let is_dir: i64 = row.get(1)?;
            let frn: i64 = row.get(2)?;
            let parent: Option<i64> = row.get(3)?;
            Ok((path, is_dir != 0, frn as u64, parent.map(|p| p as u64)))
        })?;
        for row in rows {
            let (path, is_dir, frn, parent) = row?;
            if seen.contains_key(&frn) {
                continue;
            }
            seen.insert(frn, ());
            let name = PathBuf::from(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            entries.push(MftNameEntry {
                frn,
                parent_frn: parent.unwrap_or(platform::windows::ROOT_FRN),
                name,
                attributes: if is_dir { 0x10 } else { 0 },
            });
        }
    }
    for d in deltas {
        if seen.contains_key(&d.frn) {
            continue;
        }
        seen.insert(d.frn, ());
        entries.push(MftNameEntry {
            frn: d.frn,
            parent_frn: d.parent_frn,
            name: d.name.clone(),
            attributes: d.attributes,
        });
    }
    let lookup = platform::windows::build_frn_lookup(&entries);
    let mut stats = DeltaApplyStats::default();
    for d in deltas {
        let resolved = match platform::windows::resolve_one(
            &lookup,
            &entries,
            volume_root,
            &MftNameEntry {
                frn: d.frn,
                parent_frn: d.parent_frn,
                name: d.name.clone(),
                attributes: d.attributes,
            },
        ) {
            Some(r) => r,
            None => {
                stats.skipped += 1;
                continue;
            }
        };
        match d.kind() {
            platform::windows::DeltaKind::Remove => {
                stats.removed += remove_path(conn, &resolved.path)? as u64;
            }
            platform::windows::DeltaKind::Upsert => {
                if skip_hidden && (d.attributes & 0x02 != 0 || d.name.starts_with('.')) {
                    stats.skipped += 1;
                    continue;
                }
                // Rename/move orphan fix: the range walk reports current
                // state, so a rename arrives as one upsert under the NEW
                // path. The same masked FRN under a different path means
                // the old row is dead (renamed away, or its MFT slot was
                // recycled after a delete the walk cannot report — slot
                // reuse proves the old occupant is gone). Scoped to this
                // volume root: FRN segments repeat across volumes.
                remove_frn_orphans(conn, volume_root, d.frn, &resolved.path)?;
                match stat_entry(resolved.path.clone()) {
                    Some(entry) => {
                        upsert_full(conn, &entry, Some(d.frn), Some(d.parent_frn))?;
                        stats.upserted += 1;
                    }
                    None => {
                        // Gone between journal and apply: converge to
                        // removal rather than leaving the stale row.
                        stats.removed += remove_path(conn, &resolved.path)? as u64;
                    }
                }
            }
        }
    }
    Ok(stats)
}

/// Delete one path (and, when it is a directory, everything beneath it).
///
/// `%`, `_`, and `\` in the path are escaped so names like `100%.txt`
/// cannot over-delete siblings (`100X.txt`); same rule as
/// [`remove_frn_orphans`]. Returns the number of rows removed.
pub fn remove_path(conn: &Connection, path: &Path) -> Result<usize> {
    fn like_pattern(prefix: &str, sep: char) -> String {
        let mut esc = String::with_capacity(prefix.len() + 2);
        for c in prefix.chars() {
            if c == '%' || c == '_' || c == '\\' {
                esc.push('\\');
            }
            esc.push(c);
        }
        esc.push(sep);
        esc.push('%');
        esc
    }
    let owned = path.to_string_lossy().into_owned();
    let n = conn.execute(
        "DELETE FROM files WHERE path = ?1
         OR path LIKE ?2 ESCAPE '\\' OR path LIKE ?3 ESCAPE '\\'",
        rusqlite::params![owned, like_pattern(&owned, '/'), like_pattern(&owned, '\\')],
    )?;
    Ok(n)
}

/// Delete rows holding `frn` under a different path than `keep`: the
/// rename/move orphan left when a path-keyed upsert lands under a new
/// name (or an MFT slot is recycled after a delete the range walk
/// cannot report — slot reuse proves the old occupant is gone).
/// Scoped under `volume_root` because FRN segments repeat per volume.
/// Returns the number of rows removed.
pub fn remove_frn_orphans(
    conn: &Connection,
    volume_root: &Path,
    frn: u64,
    keep: &Path,
) -> Result<usize> {
    fn like_prefix(pattern_root: &str) -> String {
        let mut root = pattern_root.to_string();
        if !root.ends_with(['\\', '/']) {
            root.push(std::path::MAIN_SEPARATOR);
        }
        let mut esc = String::with_capacity(root.len() + 1);
        for c in root.chars() {
            if c == '%' || c == '_' || c == '\\' {
                esc.push('\\');
            }
            esc.push(c);
        }
        esc.push('%');
        esc
    }
    let root = volume_root.to_string_lossy();
    let root_fwd = root.replace('\\', "/");
    let n = conn.execute(
        "DELETE FROM files WHERE frn = ?1 AND path != ?2
         AND (path LIKE ?3 ESCAPE '\\' OR path LIKE ?4 ESCAPE '\\')",
        rusqlite::params![
            frn as i64,
            keep.to_string_lossy().as_ref(),
            like_prefix(&root),
            like_prefix(&root_fwd),
        ],
    )?;
    Ok(n)
}

/// True for I/O failures that mean "access denied".
///
/// Surfaced so callers (and the scan counters below) can distinguish a
/// permission problem from other errors instead of silently dropping it.
pub fn is_permission_error(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied
}

/// Options controlling a filesystem scan.
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Root directories to walk (each walked recursively).
    pub roots: Vec<PathBuf>,
    /// Follow symbolic links while walking.
    pub follow_symlinks: bool,
    /// Skip hidden entries (platform-gated definition, see [`entry_is_hidden`]).
    pub skip_hidden: bool,
}

impl ScanOptions {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            follow_symlinks: false,
            skip_hidden: true,
        }
    }
}

/// Progress callback: invoked with the total number of entries scanned so far.
///
/// Called every [`PROGRESS_STRIDE`] entries and once more at the end of a
/// scan (so the final count is always reported, including zero). Must be
/// cheap: it runs on the thread driving the walk.
pub type ProgressCallback = dyn Fn(u64) + Send + Sync;

/// Number of scanned entries between progress-callback invocations.
pub const PROGRESS_STRIDE: u64 = 100;

/// Rayon parallelism (directory-read threads) used by the jwalk walk.
///
/// NTFS volumes tend to be metadata-latency bound and contend with realtime
/// antivirus filters, so Windows uses a moderate degree; Linux scales higher.
#[cfg(windows)]
pub const DEFAULT_PARALLELISM: usize = 8;
/// Rayon parallelism (directory-read threads) used by the jwalk walk.
///
/// Linux scales well with higher read parallelism on ext4/btrfs/tmpfs.
#[cfg(target_os = "linux")]
pub const DEFAULT_PARALLELISM: usize = 16;
/// Fallback parallelism for other platforms.
#[cfg(not(any(windows, target_os = "linux")))]
pub const DEFAULT_PARALLELISM: usize = 4;

/// Shared dotfile check used on every platform.
fn file_name_is_dotted(path: &Path) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy().starts_with('.'))
        .unwrap_or(false)
}

/// Platform-gated hidden-entry test used to filter scan results.
///
/// Windows: an entry is hidden when the file carries the
/// `FILE_ATTRIBUTE_HIDDEN` flag (jwalk's own `skip_hidden` only covers
/// dotfiles, so the attribute check must be explicit here).
#[cfg(windows)]
pub fn entry_is_hidden(path: &Path, metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    if metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0 {
        return true;
    }
    file_name_is_dotted(path)
}

/// Platform-gated hidden-entry test used to filter scan results.
///
/// Linux: hidden means a dotted file name (no attribute bit exists).
#[cfg(target_os = "linux")]
pub fn entry_is_hidden(path: &Path, _metadata: &fs::Metadata) -> bool {
    file_name_is_dotted(path)
}

/// Fallback hidden-entry test for remaining platforms (dotfile rule).
#[cfg(not(any(windows, target_os = "linux")))]
pub fn entry_is_hidden(path: &Path, _metadata: &fs::Metadata) -> bool {
    file_name_is_dotted(path)
}

fn system_time_to_unix(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn entry_from_metadata(path: PathBuf, metadata: &fs::Metadata) -> FileEntry {
    let mtime = metadata.modified().map(system_time_to_unix).unwrap_or(0);
    FileEntry::new(path, metadata.len(), mtime, metadata.is_dir())
}

/// Counters describing one [`scan_roots`] pass.
///
/// * `scanned` — entries successfully collected (always equals the number
///   of returned entries).
/// * `skipped` — entries seen but filtered out (currently the hidden-file
///   filter).
/// * `errors` — walk errors plus per-entry metadata failures. Permission
///   failures (`ErrorKind::PermissionDenied`, the "access denied" case) are
///   counted here instead of being silently dropped, so they stay visible.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanStats {
    pub scanned: u64,
    pub skipped: u64,
    pub errors: u64,
}

impl ScanStats {
    /// Total entries scanned so far (same as [`ScanStats::scanned`]).
    pub fn total_scanned(&self) -> u64 {
        self.scanned
    }
    /// Number of entries skipped by filters.
    pub fn skipped_count(&self) -> u64 {
        self.skipped
    }
    /// Number of walk/metadata errors encountered (access-denied included).
    pub fn error_count(&self) -> u64 {
        self.errors
    }
}

impl std::fmt::Display for ScanStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.scanned)
    }
}

/// The result of [`scan_roots`]: collected entries plus visibility counters.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Successfully scanned entries.
    pub entries: Vec<FileEntry>,
    /// Number of entries collected (always `entries.len()`).
    pub scanned: u64,
    /// Entries filtered out (hidden-file filter).
    pub skipped: u64,
    /// Walk + metadata errors (permission errors included).
    pub errors: u64,
}

impl ScanResult {
    /// Counter-only view of this result.
    pub fn stats(&self) -> ScanStats {
        ScanStats {
            scanned: self.scanned,
            skipped: self.skipped,
            errors: self.errors,
        }
    }
    /// Number of collected entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    /// True when no entries were collected.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// Total entries scanned (same as [`ScanResult::scanned`]).
    pub fn total_scanned(&self) -> u64 {
        self.scanned
    }
    /// Entries skipped by filters.
    pub fn skipped_count(&self) -> u64 {
        self.skipped
    }
    /// Walk/metadata errors (access-denied included).
    pub fn error_count(&self) -> u64 {
        self.errors
    }
}

/// Walk every root in parallel (jwalk rayon directory readers), collect
/// [`FileEntry`] values, and report progress through `progress`.
///
/// The progress callback receives the running total of scanned entries
/// (every [`PROGRESS_STRIDE`] entries plus a final call with the grand
/// total, including a `0` call for an empty scan). Entries that fail to
/// stat — including access-denied entries — are counted in
/// [`ScanResult::errors`]; hidden-filtered entries are counted in
/// [`ScanResult::skipped`].
pub fn scan_roots(options: &ScanOptions, progress: Option<&ProgressCallback>) -> ScanResult {
    let mut out = Vec::new();
    let mut scanned: u64 = 0;
    let mut skipped: u64 = 0;
    let mut errors: u64 = 0;
    for root in &options.roots {
        let walker = jwalk::WalkDir::new(root)
            .follow_links(options.follow_symlinks)
            .skip_hidden(false)
            .parallelism(jwalk::Parallelism::RayonNewPool(DEFAULT_PARALLELISM));
        for entry in walker.into_iter() {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    // Walk error (unreadable directory, vanished root,
                    // access denied at read_dir level): visible via count.
                    errors += 1;
                    continue;
                }
            };
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(md) => md,
                Err(_) => {
                    // Metadata failure (access denied, removed mid-walk):
                    // counted, never silently swallowed.
                    errors += 1;
                    continue;
                }
            };
            if options.skip_hidden && entry_is_hidden(&path, &metadata) {
                skipped += 1;
                continue;
            }
            out.push(entry_from_metadata(path, &metadata));
            scanned += 1;
            if let Some(cb) = progress {
                if scanned % PROGRESS_STRIDE == 0 {
                    cb(scanned);
                }
            }
        }
    }
    if let Some(cb) = progress {
        if scanned == 0 || scanned % PROGRESS_STRIDE != 0 {
            cb(scanned);
        }
    }
    ScanResult {
        entries: out,
        scanned,
        skipped,
        errors,
    }
}

/// Stat one path into a [`FileEntry`].
///
/// `None` when the path is gone or unreadable — the MFT fast path leans
/// on this to drop stale table rows instead of returning them.
pub(crate) fn stat_entry(path: PathBuf) -> Option<FileEntry> {
    let metadata = fs::symlink_metadata(&path).ok()?;
    Some(entry_from_metadata(path, &metadata))
}

/// Outcome of [`fast_search`]: fully filtered, stat'ed hits.
#[derive(Debug, Default)]
pub struct FastSearchOutcome {
    /// Final hits: prefilter survivors (MFT path) or walked entries
    /// (fallback path), fully filtered by `query` and stat'ed.
    pub entries: Vec<FileEntry>,
    /// Resolved or walked entries examined.
    pub scanned: u64,
    /// Hidden-filtered records.
    pub skipped: u64,
    /// Stat failures plus walk errors.
    pub errors: u64,
    /// True when entries came from the MFT name table.
    pub via_mft: bool,
    /// Human-readable sourcing note (volumes or fallback reason).
    pub note: String,
}

/// Search on-demand roots through the fastest available path.
///
/// On elevated Windows this enumerates the MFT name table, prefilters by
/// file name, and stats only survivors; everywhere else — and on any MFT
/// failure — it runs the tuned walk and filters the walked entries.
/// Either way the returned hits satisfy `query` exactly.
///
/// `want_metadata` skips per-file stats on the MFT path when neither the
/// query nor the output needs size/mtime (entries then carry zeros); the
/// walk fallback always stats. Pass `true` unless the caller is sure the
/// query is name-only and only paths are printed.
pub fn fast_search(
    options: &ScanOptions,
    query: &supersearch_query::Query,
    want_metadata: bool,
    progress: Option<&ProgressCallback>,
) -> FastSearchOutcome {
    #[cfg(windows)]
    {
        match platform::windows::mft_search(options, query, want_metadata, progress) {
            Ok(outcome) => {
                let hits: Vec<FileEntry> = supersearch_query::search(&outcome.entries, query)
                    .into_iter()
                    .cloned()
                    .collect();
                return FastSearchOutcome {
                    entries: hits,
                    scanned: outcome.scanned,
                    skipped: outcome.skipped,
                    errors: outcome.errors + outcome.orphans,
                    via_mft: true,
                    note: format!("MFT {} [{}]", outcome.volumes.join(","), outcome.profile),
                };
            }
            Err(reason) => {
                let result = scan_roots(options, progress);
                let hits: Vec<FileEntry> = supersearch_query::search(&result.entries, query)
                    .into_iter()
                    .cloned()
                    .collect();
                return FastSearchOutcome {
                    entries: hits,
                    scanned: result.scanned,
                    skipped: result.skipped,
                    errors: result.errors,
                    via_mft: false,
                    note: format!("walk ({reason})"),
                };
            }
        }
    }
    #[cfg(not(windows))]
    {
        let result = scan_roots(options, progress);
        let hits: Vec<FileEntry> = supersearch_query::search(&result.entries, query)
            .into_iter()
            .cloned()
            .collect();
        FastSearchOutcome {
            entries: hits,
            scanned: result.scanned,
            skipped: result.skipped,
            errors: result.errors,
            via_mft: false,
            note: "walk".to_string(),
        }
    }
}

/// Scan `options.roots` and upsert every entry into `conn` in one batch
/// transaction. Returns per-scan counters (see [`ScanStats`]).
pub fn scan_to_db(
    conn: &mut Connection,
    options: &ScanOptions,
    progress: Option<&ProgressCallback>,
) -> Result<ScanStats> {
    let result = scan_roots(options, progress);
    let tx = conn.transaction()?;
    for e in &result.entries {
        upsert(&tx, e)?;
    }
    tx.commit()?;
    Ok(result.stats())
}

/// Stat one path and upsert it into the index.
///
/// Returns `Ok(true)` when the path was indexed, `Ok(false)` when the path
/// is gone (any stale row for it is removed and nothing is upserted).
pub fn upsert_path(conn: &Connection, path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(md) => {
            upsert(conn, &entry_from_metadata(path.to_path_buf(), &md))?;
            Ok(true)
        }
        Err(_) => {
            let _ = remove_path(conn, path)?;
            Ok(false)
        }
    }
}

/// Index `root` and everything beneath it (used when the watcher reports a
/// newly created directory). Returns the number of entries upserted.
fn upsert_tree(conn: &Connection, root: &Path, skip_hidden: bool) -> Result<usize> {
    let walker = jwalk::WalkDir::new(root)
        .follow_links(false)
        .skip_hidden(false)
        .parallelism(jwalk::Parallelism::RayonNewPool(DEFAULT_PARALLELISM));
    let mut n = 0usize;
    for entry in walker.into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        let Ok(md) = fs::symlink_metadata(&path) else {
            continue;
        };
        if skip_hidden && entry_is_hidden(&path, &md) {
            continue;
        }
        upsert(conn, &entry_from_metadata(path, &md))?;
        n += 1;
    }
    Ok(n)
}

/// Apply one `notify` event to an open index connection (no hidden filter).
///
/// * Created/modified paths are statted and upserted (directories are
///   indexed recursively so a new subtree lands atomically).
/// * Removed paths are deleted (directories remove their whole subtree).
/// * Other event kinds still upsert their paths best-effort.
///
/// Returns the number of entries upserted plus rows deleted.
pub fn apply_event(conn: &Connection, event: &notify::Event) -> Result<usize> {
    apply_event_filtered(conn, event, false)
}

/// [`apply_event`] with an optional hidden-file filter matching
/// [`ScanOptions::skip_hidden`]. Hidden paths are neither upserted nor
/// counted.
pub fn apply_event_filtered(
    conn: &Connection,
    event: &notify::Event,
    skip_hidden: bool,
) -> Result<usize> {
    use notify::EventKind;
    let mut affected = 0usize;
    match &event.kind {
        EventKind::Remove(_) => {
            for p in &event.paths {
                affected += remove_path(conn, p)?;
            }
        }
        _ => {
            for p in &event.paths {
                let md = match fs::symlink_metadata(p) {
                    Ok(md) => md,
                    Err(_) => {
                        // Vanished between event and handling: drop stale row.
                        affected += remove_path(conn, p)?;
                        continue;
                    }
                };
                if skip_hidden && entry_is_hidden(p, &md) {
                    continue;
                }
                if md.is_dir() {
                    affected += upsert_tree(conn, p, skip_hidden)?;
                } else if upsert_path(conn, p)? {
                    affected += 1;
                }
            }
        }
    }
    Ok(affected)
}

/// Live watcher over the scan roots with incremental re-indexing.
///
/// Channel mode ([`WatcherStub::start`]) forwards every `notify` event into
/// an internal queue instead of dropping it; call [`WatcherStub::drain_events`]
/// or [`WatcherStub::apply_pending`] to consume it. Database mode
/// ([`WatcherStub::start_with_db`]) applies each event to the SQLite index
/// at `db_path` directly on the notify thread (one short-lived connection
/// per event, since `rusqlite::Connection` is `Send` but not `Sync`).
pub struct WatcherStub {
    _watcher: notify::RecommendedWatcher,
    /// Roots under watch (recursive).
    pub roots: Vec<PathBuf>,
    /// Index file updated per event in database mode; `None` in channel mode.
    pub db_path: Option<PathBuf>,
    /// Hidden-file filtering applied to incremental upserts.
    pub skip_hidden: bool,
    events: Option<std::sync::mpsc::Receiver<notify::Result<notify::Event>>>,
}

impl WatcherStub {
    /// Watch `options.roots` and queue events for later incremental indexing.
    ///
    /// No event is dropped: use [`WatcherStub::apply_pending`] (or
    /// [`WatcherStub::drain_events`] + [`apply_event_filtered`]) to fold
    /// them into an open connection.
    pub fn start(options: &ScanOptions) -> notify::Result<Self> {
        use notify::Watcher;
        let roots = options.roots.clone();
        let skip_hidden = options.skip_hidden;
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let _ = tx.send(res);
            })?;
        for root in &roots {
            watcher.watch(root, notify::RecursiveMode::Recursive)?;
        }
        Ok(Self {
            _watcher: watcher,
            roots,
            db_path: None,
            skip_hidden,
            events: Some(rx),
        })
    }

    /// Watch `options.roots` and incrementally upsert each filesystem event
    /// into the SQLite index at `db_path`.
    pub fn start_with_db(options: &ScanOptions, db_path: PathBuf) -> notify::Result<Self> {
        use notify::Watcher;
        let roots = options.roots.clone();
        let skip_hidden = options.skip_hidden;
        let db = db_path.clone();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                match res {
                    Ok(ev) => {
                        // Best-effort: a transient lock or missing file must
                        // never kill the watcher thread.
                        if let Ok(conn) = Connection::open(&db) {
                            let _ = apply_event_filtered(&conn, &ev, skip_hidden);
                        }
                    }
                    Err(_) => {}
                }
            })?;
        for root in &roots {
            watcher.watch(root, notify::RecursiveMode::Recursive)?;
        }
        Ok(Self {
            _watcher: watcher,
            roots,
            db_path: Some(db_path),
            skip_hidden,
            events: None,
        })
    }

    /// Non-blocking drain of queued events (channel mode; empty in db mode).
    pub fn drain_events(&self) -> Vec<notify::Result<notify::Event>> {
        let mut out = Vec::new();
        if let Some(rx) = self.events.as_ref() {
            while let Ok(ev) = rx.try_recv() {
                out.push(ev);
            }
        }
        out
    }

    /// Pop one queued event without blocking (channel mode).
    pub fn try_recv(&self) -> Option<notify::Result<notify::Event>> {
        self.events.as_ref().and_then(|rx| rx.try_recv().ok())
    }

    /// Drain queued events and fold them into `conn` via
    /// [`apply_event_filtered`]. Returns entries upserted + rows deleted.
    pub fn apply_pending(&self, conn: &Connection) -> Result<usize> {
        let mut affected = 0usize;
        for res in self.drain_events() {
            if let Ok(ev) = res {
                affected += apply_event_filtered(conn, &ev, self.skip_hidden)?;
            }
        }
        Ok(affected)
    }
}

/// Point-in-time status of the background polling daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatus {
    /// Wall-clock time of the last successful index write (`None` before the
    /// first success).
    pub last_indexed: Option<SystemTime>,
    /// True when the index cannot be trusted: no successful scan yet, or the
    /// most recent tick failed.
    pub stale: bool,
}

impl DaemonStatus {
    /// `last_indexed` as Unix seconds (`None` before the first success).
    pub fn last_indexed_secs(&self) -> Option<u64> {
        self.last_indexed
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
    }
}

/// Background daemon: re-runs [`scan_to_db`] every `interval` until stopped.
///
/// Each tick opens its own connection (rusqlite connections are `Send` but
/// not `Sync`, so sharing one across the thread boundary is not an option).
/// Successful ticks refresh [`PollHandle::last_indexed`] and clear the stale
/// flag; failed ticks set the stale flag and leave the timestamp untouched.
pub struct PollHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    last_indexed: Arc<Mutex<Option<SystemTime>>>,
    stale: Arc<AtomicBool>,
}

impl PollHandle {
    /// Signal the polling thread to exit after its current tick/sleep.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Signal stop and wait for the thread to exit.
    pub fn stop_and_join(mut self) {
        self.stop();
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }

    /// Wall-clock time of the last successful index write.
    pub fn last_indexed(&self) -> Option<SystemTime> {
        self.last_indexed.lock().ok().and_then(|g| *g)
    }

    /// `last_indexed` as Unix seconds (`None` before the first success).
    pub fn last_indexed_secs(&self) -> Option<u64> {
        self.last_indexed()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
    }

    /// True when the index cannot be trusted (no success yet or last tick
    /// failed).
    pub fn is_stale(&self) -> bool {
        self.stale.load(Ordering::Relaxed)
    }

    /// Alias for [`PollHandle::is_stale`].
    pub fn stale(&self) -> bool {
        self.is_stale()
    }

    /// Point-in-time snapshot of daemon status.
    pub fn status(&self) -> DaemonStatus {
        DaemonStatus {
            last_indexed: self.last_indexed(),
            stale: self.is_stale(),
        }
    }
}

/// Spawn the daemon polling thread described by [`PollHandle`].
pub fn spawn_poll_thread(db_path: PathBuf, options: ScanOptions, interval: Duration) -> PollHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let last_indexed = Arc::new(Mutex::new(None));
    let last_writer = Arc::clone(&last_indexed);
    let stale = Arc::new(AtomicBool::new(true));
    let stale_writer = Arc::clone(&stale);
    let thread = thread::Builder::new()
        .name("supersearch-index-poll".to_string())
        .spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                let tick_ok = (|| -> Option<()> {
                    let mut conn = Connection::open(&db_path).ok()?;
                    init_db(&conn).ok()?;
                    scan_to_db(&mut conn, &options, None).ok()?;
                    Some(())
                })()
                .is_some();
                if tick_ok {
                    if let Ok(mut g) = last_writer.lock() {
                        *g = Some(SystemTime::now());
                    }
                    stale_writer.store(false, Ordering::Relaxed);
                } else {
                    stale_writer.store(true, Ordering::Relaxed);
                }
                // Sleep in small chunks so stop() stays responsive.
                let mut waited = Duration::ZERO;
                while waited < interval && !flag.load(Ordering::Relaxed) {
                    let step = Duration::from_millis(100).min(interval - waited);
                    thread::sleep(step);
                    waited += step;
                }
            }
        })
        .expect("spawn supersearch-index poll thread");
    PollHandle {
        stop,
        thread: Some(thread),
        last_indexed,
        stale,
    }
}

/// Fill stats for [`index_fill_mft`].
#[cfg(windows)]
#[derive(Debug, Default)]
pub struct IndexFillStats {
    /// Rows written with FRN identity.
    pub rows: u64,
    /// Records examined / hidden-filtered / stat-failed / orphaned.
    pub scanned: u64,
    pub skipped: u64,
    pub errors: u64,
    pub orphans: u64,
    /// Stale rows removed (delete tombstoning).
    pub tombstoned: u64,
    /// Volumes filled through the MFT path.
    pub volumes: Vec<String>,
    /// Phase timings, or the walk-fallback reason.
    pub profile: String,
}

/// Remove rows under `roots` that are absent from the just-completed
/// fill (paths banked in the `fresh_fill` temp table): the delete
/// tombstoning the ENUM-range deltas cannot provide, since deleted
/// files leave no MFT record to enumerate. Set-based anti-join, one
/// statement per root. Returns rows removed.
#[cfg(windows)]
fn remove_absent_under_roots(tx: &rusqlite::Transaction, roots: &[PathBuf]) -> Result<u64> {
    fn like_esc(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if c == '\\' || c == '%' || c == '_' {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }
    let mut removed = 0u64;
    for root in roots {
        let owned = root.to_string_lossy().into_owned();
        let fwd = owned.replace('\\', "/");
        let esc = like_esc(&owned);
        let esc_fwd = like_esc(&fwd);
        removed += tx.execute(
            "DELETE FROM files WHERE
             (path = ?1 OR path = ?2
              OR path LIKE ?3 ESCAPE '\\' OR path LIKE ?4 ESCAPE '\\'
              OR path LIKE ?5 ESCAPE '\\' OR path LIKE ?6 ESCAPE '\\')
             AND path NOT IN (SELECT path FROM fresh_fill)",
            rusqlite::params![
                owned,
                fwd,
                format!("{esc}/%"),
                format!("{esc}\\\\%"),
                format!("{esc_fwd}/%"),
                format!("{esc_fwd}\\\\%"),
            ],
        )? as u64;
    }
    Ok(removed)
}

/// Full MFT-backed index fill with FRN identity and fresh cursors.
///
/// Enumerates every resolvable record under `options.roots`, upserts
/// rows with [`upsert_full`] inside one transaction (also much faster
/// than a commit per row), banks every fresh path, then tombstones
/// rows the fill no longer sees via [`remove_absent_under_roots`]
/// (deletes leave no MFT record, so only a full sync converges them),
/// and stores each volume's current journal head via
/// [`save_usn_cursor`] so later ticks read deltas. When the MFT path
/// is unavailable the fill degrades to [`scan_to_db`] (rows without
/// FRNs, no cursors, no tombstoning) and reports it in `profile`.
#[cfg(windows)]
pub fn index_fill_mft(
    conn: &mut Connection,
    options: &ScanOptions,
    progress: Option<&ProgressCallback>,
) -> Result<IndexFillStats> {
    let fill = match platform::windows::mft_index_full(options, progress) {
        Ok(fill) => fill,
        Err(reason) => {
            let stats = scan_to_db(conn, options, progress)?;
            return Ok(IndexFillStats {
                rows: stats.scanned,
                scanned: stats.scanned,
                skipped: stats.skipped,
                errors: stats.errors,
                orphans: 0,
                tombstoned: 0,
                volumes: Vec::new(),
                profile: format!("walk fallback ({reason})"),
            });
        }
    };
    let tx = conn.transaction()?;
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS fresh_fill(path TEXT PRIMARY KEY);
         DELETE FROM fresh_fill;",
    )?;
    {
        let mut bank = tx.prepare("INSERT OR IGNORE INTO fresh_fill(path) VALUES (?1)")?;
        for row in &fill.rows {
            upsert_full(&tx, &row.entry, Some(row.frn), Some(row.parent_frn))?;
            bank.execute(rusqlite::params![row.entry.path.to_string_lossy().as_ref()])?;
        }
    }
    let tombstoned = remove_absent_under_roots(&tx, &options.roots)?;
    tx.execute_batch("DROP TABLE fresh_fill;")?;
    tx.commit()?;
    for volume in &fill.volumes {
        if let Some(state) = platform::windows::current_journal_state(volume) {
            save_usn_cursor(conn, volume, state.journal_id, state.next_usn)?;
        }
    }
    Ok(IndexFillStats {
        rows: fill.rows.len() as u64,
        scanned: fill.scanned,
        skipped: fill.skipped,
        errors: fill.errors,
        orphans: fill.orphans,
        tombstoned,
        volumes: fill.volumes.clone(),
        profile: fill.profile.clone(),
    })
}

/// One USN-maintained daemon tick over a single volume.
#[cfg(windows)]
#[derive(Debug)]
pub enum UsnTickOutcome {
    /// Journal head has not moved: index is current.
    Current,
    /// Deltas applied; cursor advanced.
    Applied {
        /// Rows upserted.
        upserted: u64,
        /// Rows removed.
        removed: u64,
        /// Deltas skipped (orphans, hidden).
        skipped: u64,
        /// Stored cursor after the batch.
        next_usn: i64,
    },
    /// No cursor yet, or the journal wrapped: caller must run
    /// [`index_fill_mft`] before ticking again.
    NeedsFullScan {
        /// Human-readable cause.
        reason: String,
    },
    /// Not elevated / non-local / unhealthy: caller keeps walk-polling.
    Unsupported {
        /// Human-readable cause.
        reason: String,
    },
}

/// Advance one volume's index from its stored cursor.
///
/// Reads up to 256 journal batches, applies them with
/// [`apply_usn_deltas`], and stores the advanced cursor. Pure
/// bookkeeping around the platform reader: no threads, no sleeps.
#[cfg(windows)]
pub fn usn_tick(
    conn: &Connection,
    volume_root: &Path,
    volume: &str,
    skip_hidden: bool,
) -> Result<UsnTickOutcome> {
    use platform::windows::UsnCursor;
    let stored = match load_usn_cursor(conn, volume)? {
        Some(c) => c,
        None => {
            return Ok(UsnTickOutcome::NeedsFullScan {
                reason: "no stored cursor".to_string(),
            });
        }
    };
    let cursor = UsnCursor {
        journal_id: stored.journal_id,
        next_usn: stored.next_usn,
    };
    let read = match platform::windows::read_usn_deltas(volume, cursor, 256) {
        Ok(outcome) => outcome,
        Err(reason) => {
            return Ok(UsnTickOutcome::Unsupported {
                reason: format!("{reason}"),
            });
        }
    };
    match read {
        platform::windows::DeltaReadOutcome::NoChange => Ok(UsnTickOutcome::Current),
        platform::windows::DeltaReadOutcome::RescanRequired { reason } => {
            Ok(UsnTickOutcome::NeedsFullScan {
                reason: format!("{reason:?}"),
            })
        }
        platform::windows::DeltaReadOutcome::Deltas { deltas, next_usn } => {
            let stats = apply_usn_deltas(conn, volume_root, skip_hidden, &deltas)?;
            save_usn_cursor(conn, volume, stored.journal_id, next_usn)?;
            Ok(UsnTickOutcome::Applied {
                upserted: stats.upserted,
                removed: stats.removed,
                skipped: stats.skipped,
                next_usn,
            })
        }
    }
}

/// Target cadence between daemon full syncs: deletes leave no MFT
/// record for the range-walk deltas to report, so only a full
/// [`index_fill_mft`] (with tombstoning) converges them.
#[cfg(windows)]
pub const DAEMON_FULL_SYNC_SECS: u64 = 1800;

/// Spawn the USN-maintained index daemon: every `interval` it advances
/// each local volume from its stored cursor ([`usn_tick`]), running a
/// full [`index_fill_mft`] wherever no cursor exists or the journal
/// wrapped, plus one periodic full sync every [`DAEMON_FULL_SYNC_SECS`]
/// so file deletes (invisible to range-walk deltas) converge.
/// Non-local roots and non-Windows platforms keep the plain
/// [`scan_to_db`] polling tick. Same [`PollHandle`] contract as
/// [`spawn_poll_thread`]: per-tick success refreshes `last_indexed` and
/// clears stale; any failure sets stale.
pub fn spawn_index_thread(
    db_path: PathBuf,
    options: ScanOptions,
    interval: Duration,
) -> PollHandle {
    #[cfg(not(windows))]
    {
        return spawn_poll_thread(db_path, options, interval);
    }
    #[cfg(windows)]
    {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let last_indexed = Arc::new(Mutex::new(None));
        let last_writer = Arc::clone(&last_indexed);
        let stale = Arc::new(AtomicBool::new(true));
        let stale_writer = Arc::clone(&stale);
        let full_every_ticks = (DAEMON_FULL_SYNC_SECS / interval.as_secs().max(1)).max(1);
        let thread = thread::Builder::new()
            .name("supersearch-index-usn".to_string())
            .spawn(move || {
                let mut tick: u64 = 0;
                while !flag.load(Ordering::Relaxed) {
                    tick = tick.wrapping_add(1);
                    let full_sync = tick % full_every_ticks == 0;
                    let tick_ok = (|| -> Option<()> {
                        let mut conn = Connection::open(&db_path).ok()?;
                        init_db(&conn).ok()?;
                        // Group roots by volume; non-local roots share
                        // one walk tick.
                        use std::collections::BTreeMap;
                        let mut by_volume: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
                        let mut nonlocal = Vec::new();
                        for root in &options.roots {
                            match platform::windows::volume_for_root(root) {
                                Some(v) => by_volume.entry(v).or_default().push(root.clone()),
                                None => nonlocal.push(root.clone()),
                            }
                        }
                        if !nonlocal.is_empty() {
                            let walk_opts = ScanOptions {
                                roots: nonlocal,
                                follow_symlinks: options.follow_symlinks,
                                skip_hidden: options.skip_hidden,
                            };
                            scan_to_db(&mut conn, &walk_opts, None).ok()?;
                        }
                        for (volume, roots) in &by_volume {
                            let anchor = PathBuf::from(format!(
                                "{}:\\",
                                volume.chars().nth(4).unwrap_or('C').to_ascii_uppercase()
                            ));
                            if full_sync {
                                // Periodic delete-converging sync: the
                                // range-walk deltas cannot see deletes.
                                let fill_opts = ScanOptions {
                                    roots: roots.clone(),
                                    follow_symlinks: options.follow_symlinks,
                                    skip_hidden: options.skip_hidden,
                                };
                                index_fill_mft(&mut conn, &fill_opts, None).ok()?;
                                continue;
                            }
                            match usn_tick(&conn, &anchor, volume, options.skip_hidden).ok()? {
                                UsnTickOutcome::Current | UsnTickOutcome::Applied { .. } => {}
                                UsnTickOutcome::NeedsFullScan { .. } => {
                                    let fill_opts = ScanOptions {
                                        roots: roots.clone(),
                                        follow_symlinks: options.follow_symlinks,
                                        skip_hidden: options.skip_hidden,
                                    };
                                    index_fill_mft(&mut conn, &fill_opts, None).ok()?;
                                }
                                UsnTickOutcome::Unsupported { .. } => {
                                    let walk_opts = ScanOptions {
                                        roots: roots.clone(),
                                        follow_symlinks: options.follow_symlinks,
                                        skip_hidden: options.skip_hidden,
                                    };
                                    scan_to_db(&mut conn, &walk_opts, None).ok()?;
                                }
                            }
                        }
                        Some(())
                    })()
                    .is_some();
                    if tick_ok {
                        if let Ok(mut g) = last_writer.lock() {
                            *g = Some(SystemTime::now());
                        }
                        stale_writer.store(false, Ordering::Relaxed);
                    } else {
                        stale_writer.store(true, Ordering::Relaxed);
                    }
                    let mut waited = Duration::ZERO;
                    while waited < interval && !flag.load(Ordering::Relaxed) {
                        let step = Duration::from_millis(100).min(interval - waited);
                        thread::sleep(step);
                        waited += step;
                    }
                }
            })
            .expect("spawn supersearch-index USN thread");
        PollHandle {
            stop,
            thread: Some(thread),
            last_indexed,
            stale,
        }
    }
}

/// Load every entry from an open index database, ordered by path.
pub fn load_all(conn: &Connection) -> Result<Vec<FileEntry>> {
    let mut stmt = conn.prepare("SELECT path, size, mtime, is_dir FROM files ORDER BY path")?;
    let rows = stmt.query_map([], |row| {
        let path: String = row.get(0)?;
        let size: i64 = row.get(1)?;
        let mtime: i64 = row.get(2)?;
        let is_dir: i64 = row.get(3)?;
        Ok(FileEntry::new(
            PathBuf::from(path),
            size.max(0) as u64,
            mtime,
            is_dir != 0,
        ))
    })?;
    rows.collect()
}

/// Open the index database at `path` (creating schema if needed) and load
/// every entry. Used by the CLI/GUI "indexed" search mode.
pub fn load_db(path: &Path) -> Result<Vec<FileEntry>> {
    let conn = Connection::open(path)?;
    init_db(&conn)?;
    load_all(&conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex as StdMutex;

    fn write_file(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, contents).unwrap();
        p
    }

    fn options_for(root: &Path) -> ScanOptions {
        ScanOptions {
            roots: vec![root.to_path_buf()],
            follow_symlinks: false,
            skip_hidden: true,
        }
    }

    /// Fresh non-hidden scan root inside a tempdir.
    ///
    /// (tempfile dirs themselves use a dotted `.tmp` prefix, so tests scan a
    /// child dir to keep hidden-filter expectations deterministic.)
    fn scan_root(tmp: &tempfile::TempDir) -> PathBuf {
        let root = tmp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn paths_of(entries: &[FileEntry]) -> HashSet<PathBuf> {
        entries.iter().map(|e| e.path.clone()).collect()
    }

    #[test]
    fn scan_collects_files_and_reports_progress_total() {
        let tmp = tempfile::tempdir().unwrap();
        let root = scan_root(&tmp);
        write_file(&root, "a.txt", b"hello");
        write_file(&root, "sub/b.txt", b"world");
        fs::create_dir_all(root.join("empty_dir")).unwrap();

        let seen = Arc::new(StdMutex::new(Vec::<u64>::new()));
        let progress = {
            let seen = Arc::clone(&seen);
            move |n: u64| {
                seen.lock().unwrap().push(n);
            }
        };
        let result = scan_roots(&options_for(&root), Some(&progress));

        // root + 2 files + subdir + empty dir.
        assert_eq!(result.scanned, result.entries.len() as u64);
        assert_eq!(result.entries.len(), 5, "entries: {:?}", result.entries);
        assert_eq!(result.errors, 0);
        assert_eq!(result.skipped, 0);
        let set = paths_of(&result.entries);
        assert!(set.contains(&root.join("a.txt")));
        assert!(set.contains(&root.join("sub/b.txt")));
        assert!(set.contains(&root.join("sub")));

        // Progress callback ends with the total-scanned count.
        let seen = seen.lock().unwrap();
        assert!(!seen.is_empty(), "progress must be reported");
        assert_eq!(*seen.last().unwrap(), result.scanned);
        for w in seen.windows(2) {
            assert!(w[1] >= w[0], "progress totals must not decrease");
        }
    }

    #[test]
    fn scan_only_hidden_reports_filtered_with_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let root = scan_root(&tmp);
        write_file(&root, ".hidden", b"x");
        let seen = Arc::new(StdMutex::new(Vec::<u64>::new()));
        let progress = {
            let seen = Arc::clone(&seen);
            move |n: u64| {
                seen.lock().unwrap().push(n);
            }
        };
        // Only the non-hidden root itself is collected; the dotfile is
        // counted as skipped and progress still ends with the total.
        let result = scan_roots(&options_for(&root), Some(&progress));
        assert_eq!(result.scanned, 1);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.skipped, 1, "dotfile must be counted as skipped");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.as_slice(), &[1]);
    }

    #[test]
    fn scan_hidden_counts_skipped_and_flag_includes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = scan_root(&tmp);
        write_file(&root, "visible.txt", b"v");
        write_file(&root, ".secret.txt", b"s");

        let filtered = scan_roots(&options_for(&root), None);
        assert_eq!(filtered.skipped, 1);
        assert!(!paths_of(&filtered.entries).contains(&root.join(".secret.txt")));

        let mut opts = options_for(&root);
        opts.skip_hidden = false;
        let unfiltered = scan_roots(&opts, None);
        assert_eq!(unfiltered.skipped, 0);
        assert!(paths_of(&unfiltered.entries).contains(&root.join(".secret.txt")));
    }

    #[test]
    fn scan_missing_root_counts_errors_instead_of_panicking() {
        let missing = PathBuf::from("definitely-missing-supersearch-root-xyz");
        if missing.exists() {
            fs::remove_dir_all(&missing).ok();
        }
        let opts = ScanOptions::new(vec![missing]);
        let seen = Arc::new(StdMutex::new(Vec::<u64>::new()));
        let progress = {
            let seen = Arc::clone(&seen);
            move |n: u64| {
                seen.lock().unwrap().push(n);
            }
        };
        let result = scan_roots(&opts, Some(&progress));
        // Access-denied-visible: the failure is counted, not dropped.
        assert!(result.errors >= 1, "missing root must count errors");
        assert_eq!(result.scanned, 0);
        assert!(result.entries.is_empty());
        // Progress still fires exactly once with the zero total.
        assert_eq!(seen.lock().unwrap().as_slice(), &[0]);
        assert_eq!(result.stats().error_count(), result.errors);
    }

    #[test]
    fn old_schema_migrates_keeping_rows_and_gaining_frn_columns() {
        // A database written before the FRN columns existed.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files (
                path TEXT PRIMARY KEY,
                size INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                is_dir INTEGER NOT NULL
            )",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO files (path, size, mtime, is_dir) VALUES ('a.txt', 1, 2, 0)",
            [],
        )
        .unwrap();
        init_db(&conn).unwrap();
        // Old row preserved, new columns read back as NULL.
        let (size, frn): (i64, Option<i64>) = conn
            .query_row(
                "SELECT size, frn FROM files WHERE path = 'a.txt'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(size, 1);
        assert_eq!(frn, None);
        // ... and the migrated table accepts FRN-carrying upserts.
        let entry = supersearch_core::FileEntry::new(PathBuf::from("b.txt"), 3, 4, false);
        upsert_full(&conn, &entry, Some(11), Some(5)).unwrap();
        let (frn, parent): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT frn, parent_frn FROM files WHERE path = 'b.txt'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((frn, parent), (Some(11), Some(5)));
        // A later walk without FRNs clears the identity instead of
        // leaving a stale one behind.
        upsert(&conn, &entry).unwrap();
        let frn: Option<i64> = conn
            .query_row("SELECT frn FROM files WHERE path = 'b.txt'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(frn, None);
    }

    #[test]
    fn usn_cursor_roundtrips_per_volume() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        assert_eq!(load_usn_cursor(&conn, r"\\.\C:").unwrap(), None);
        let saved = save_usn_cursor(&conn, r"\\.\C:", 7, 100).unwrap();
        assert_eq!(saved.journal_id, 7);
        assert_eq!(saved.next_usn, 100);
        assert_eq!(
            load_usn_cursor(&conn, r"\\.\C:")
                .unwrap()
                .map(|c| (c.journal_id, c.next_usn)),
            Some((7, 100))
        );
        // Replace, and other volumes stay independent.
        save_usn_cursor(&conn, r"\\.\C:", 7, 250).unwrap();
        save_usn_cursor(&conn, r"\\.\D:", 9, 10).unwrap();
        assert_eq!(
            load_usn_cursor(&conn, r"\\.\C:")
                .unwrap()
                .map(|c| c.next_usn),
            Some(250)
        );
        assert_eq!(
            load_usn_cursor(&conn, r"\\.\D:")
                .unwrap()
                .map(|c| (c.journal_id, c.next_usn)),
            Some((9, 10))
        );
    }

    #[cfg(windows)]
    fn seed_frn_index(conn: &Connection, dir: &Path, dir_frn: u64) {
        // Index knows the directory itself (child of the volume root).
        let entry = supersearch_core::FileEntry::new(dir.to_path_buf(), 0, 0, true);
        upsert_full(conn, &entry, Some(dir_frn), Some(5)).unwrap();
    }

    #[cfg(windows)]
    fn delta(frn: u64, parent: u64, name: &str, reason: u32) -> platform::windows::UsnDelta {
        platform::windows::UsnDelta {
            frn,
            parent_frn: parent,
            name: name.to_string(),
            attributes: 0x20,
            usn: 1,
            reason,
        }
    }

    #[test]
    #[cfg(windows)]
    fn applier_upserts_creates_and_deletes_removes() {
        use platform::windows::{USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vol");
        fs::create_dir(&root).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        seed_frn_index(&conn, &root, 100);
        // Create arrives: real file exists, so it upserts with identity.
        fs::write(root.join("new.txt"), b"hi").unwrap();
        let stats = apply_usn_deltas(
            &conn,
            tmp.path(),
            true,
            &[delta(101, 100, "new.txt", USN_REASON_FILE_CREATE)],
        )
        .unwrap();
        assert_eq!(stats.upserted, 1);
        let frn: Option<i64> = conn
            .query_row(
                "SELECT frn FROM files WHERE path LIKE '%new.txt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(frn, Some(101));
        // Delete arrives after the file is gone: the row vanishes.
        fs::remove_file(root.join("new.txt")).unwrap();
        let stats = apply_usn_deltas(
            &conn,
            tmp.path(),
            true,
            &[delta(101, 100, "new.txt", USN_REASON_FILE_DELETE)],
        )
        .unwrap();
        assert_eq!(stats.removed, 1);
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path LIKE '%new.txt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    #[cfg(windows)]
    fn applier_rename_pair_moves_the_row() {
        use platform::windows::{USN_REASON_RENAME_NEW, USN_REASON_RENAME_OLD};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vol");
        fs::create_dir(&root).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        seed_frn_index(&conn, &root, 100);
        // Old path indexed; the rename already happened on disk.
        let old = supersearch_core::FileEntry::new(root.join("old.txt"), 1, 0, false);
        upsert_full(&conn, &old, Some(101), Some(100)).unwrap();
        fs::write(root.join("new.txt"), b"x").unwrap();
        let stats = apply_usn_deltas(
            &conn,
            tmp.path(),
            true,
            &[
                delta(101, 100, "old.txt", USN_REASON_RENAME_OLD),
                delta(101, 100, "new.txt", USN_REASON_RENAME_NEW),
            ],
        )
        .unwrap();
        assert_eq!(stats.removed, 1);
        assert_eq!(stats.upserted, 1);
        let mut stmt = conn
            .prepare("SELECT path FROM files WHERE path LIKE '%.txt'")
            .unwrap();
        let paths: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(paths.iter().any(|p| p.ends_with("new.txt")));
        assert!(!paths.iter().any(|p| p.ends_with("old.txt")));
    }

    #[test]
    #[cfg(windows)]
    fn applier_single_upsert_clears_frn_orphan() {
        // ENUM-range reality: a rename arrives as ONE upsert under the
        // new name (reason flags are zero), never as an old/new pair.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vol");
        fs::create_dir(&root).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        seed_frn_index(&conn, &root, 100);
        // Old path indexed; the rename already happened on disk.
        let old = supersearch_core::FileEntry::new(root.join("old.txt"), 1, 0, false);
        upsert_full(&conn, &old, Some(101), Some(100)).unwrap();
        fs::write(root.join("new.txt"), b"x").unwrap();
        let stats =
            apply_usn_deltas(&conn, tmp.path(), true, &[delta(101, 100, "new.txt", 0)]).unwrap();
        assert_eq!(stats.upserted, 1);
        let mut stmt = conn
            .prepare("SELECT path FROM files WHERE path LIKE '%.txt'")
            .unwrap();
        let paths: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(paths.iter().any(|p| p.ends_with("new.txt")));
        assert!(
            !paths.iter().any(|p| p.ends_with("old.txt")),
            "renamed-away path must not survive: {paths:?}"
        );
    }

    #[test]
    #[cfg(windows)]
    fn fill_tombstones_rows_absent_from_fresh_set() {
        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        for p in [
            r"C:\keep\a.txt",
            r"C:\keep\gone.txt",
            r"C:\keep\sub\deep.txt",
            r"C:\other\b.txt",
            r"C:\we%ird\a.txt",
            r"C:\weXird\b.txt",
        ] {
            upsert(
                &conn,
                &supersearch_core::FileEntry::new(PathBuf::from(p), 1, 0, false),
            )
            .unwrap();
        }
        let tx = conn.transaction().unwrap();
        tx.execute_batch("CREATE TEMP TABLE fresh_fill(path TEXT PRIMARY KEY)")
            .unwrap();
        {
            let mut bank = tx
                .prepare("INSERT INTO fresh_fill(path) VALUES (?1)")
                .unwrap();
            // gone.txt deleted; other roots keep everything they had.
            for p in [
                r"C:\keep\a.txt",
                r"C:\keep\sub\deep.txt",
                r"C:\other\b.txt",
                r"C:\we%ird\a.txt",
                r"C:\weXird\b.txt",
            ] {
                bank.execute(rusqlite::params![p]).unwrap();
            }
        }
        // `%` in a root is literal: C:\we%ird must not match C:\weXird.
        let removed = remove_absent_under_roots(
            &tx,
            &[PathBuf::from(r"C:\keep"), PathBuf::from(r"C:\we%ird")],
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(removed, 1);
        let left: Vec<String> = conn
            .prepare("SELECT path FROM files ORDER BY path")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            left,
            vec![
                r"C:\keep\a.txt".to_string(),
                r"C:\keep\sub\deep.txt".to_string(),
                r"C:\other\b.txt".to_string(),
                r"C:\we%ird\a.txt".to_string(),
                r"C:\weXird\b.txt".to_string(),
            ]
        );
    }

    #[test]
    fn scan_to_db_roundtrip_and_stats() {
        let tmp = tempfile::tempdir().unwrap();
        let root = scan_root(&tmp);
        write_file(&root, "one.txt", b"1");
        write_file(&root, "two.txt", b"22");
        let mut conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        let stats = scan_to_db(&mut conn, &options_for(&root), None).unwrap();
        assert_eq!(stats.scanned, 3); // root + 2 files
        assert_eq!(stats.errors, 0);
        assert_eq!(stats.skipped, 0);
        assert_eq!(stats.total_scanned(), stats.scanned);
        let loaded = load_all(&conn).unwrap();
        assert_eq!(loaded.len() as u64, stats.scanned);
        let set = paths_of(&loaded);
        assert!(set.contains(&root.join("one.txt")));
    }

    #[test]
    fn incremental_upsert_and_remove_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();

        let f = write_file(tmp.path(), "live.txt", b"v1");
        assert!(upsert_path(&conn, &f).unwrap());
        assert!(paths_of(&load_all(&conn).unwrap()).contains(&f));

        // Removing the file then upserting reports gone and drops the row.
        fs::remove_file(&f).unwrap();
        assert!(!upsert_path(&conn, &f).unwrap());
        assert!(!paths_of(&load_all(&conn).unwrap()).contains(&f));

        // remove_path deletes a directory subtree.
        let nested = write_file(tmp.path(), "tree/child.txt", b"c");
        upsert_path(&conn, &nested).unwrap();
        upsert_path(&conn, &tmp.path().join("tree")).unwrap();
        let removed = remove_path(&conn, &tmp.path().join("tree")).unwrap();
        assert!(removed >= 2);
        let set = paths_of(&load_all(&conn).unwrap());
        assert!(!set.contains(&nested));
    }

    /// `%` in a name must not wildcard-match siblings: removing
    /// `100%.txt` leaves `100X.txt` alone.
    #[test]
    fn remove_path_escapes_like_wildcards() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();

        let pct = write_file(tmp.path(), "100%.txt", b"p");
        let other = write_file(tmp.path(), "100X.txt", b"x");
        assert!(upsert_path(&conn, &pct).unwrap());
        assert!(upsert_path(&conn, &other).unwrap());
        assert_eq!(remove_path(&conn, &pct).unwrap(), 1);
        let set = paths_of(&load_all(&conn).unwrap());
        assert!(!set.contains(&pct));
        assert!(set.contains(&other));
    }

    fn notify_event(kind: notify::EventKind, paths: Vec<PathBuf>) -> notify::Event {
        notify::Event {
            kind,
            paths,
            attrs: Default::default(),
        }
    }

    #[test]
    fn apply_event_upserts_creates_and_deletes_removes() {
        use notify::event::{CreateKind, RemoveKind};
        use notify::EventKind;

        let tmp = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();

        let f = write_file(tmp.path(), "ev.txt", b"data");
        let created = notify_event(EventKind::Create(CreateKind::File), vec![f.clone()]);
        assert!(apply_event(&conn, &created).unwrap() >= 1);
        assert!(paths_of(&load_all(&conn).unwrap()).contains(&f));

        fs::remove_file(&f).unwrap();
        let removed = notify_event(EventKind::Remove(RemoveKind::File), vec![f.clone()]);
        apply_event(&conn, &removed).unwrap();
        assert!(!paths_of(&load_all(&conn).unwrap()).contains(&f));
    }

    #[test]
    fn watcher_channel_applies_filesystem_events() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();

        let watcher = WatcherStub::start(&options_for(tmp.path())).unwrap();
        // Give the OS watcher a moment to arm before generating events.
        std::thread::sleep(Duration::from_millis(500));
        let f = write_file(tmp.path(), "watched.txt", b"w");

        // Wait (bounded) for at least one event mentioning the new file, then
        // fold everything pending into the index.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut saw = false;
        while std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            for res in watcher.drain_events() {
                if let Ok(ev) = &res {
                    if ev
                        .paths
                        .iter()
                        .any(|p| p == &f || p.starts_with(tmp.path()))
                    {
                        saw = true;
                    }
                    let _ = apply_event_filtered(&conn, ev, true);
                }
            }
            if saw && paths_of(&load_all(&conn).unwrap()).contains(&f) {
                break;
            }
        }
        assert!(saw, "watcher must deliver events instead of dropping them");
        assert!(
            paths_of(&load_all(&conn).unwrap()).contains(&f),
            "incremental upsert must index the new file"
        );
    }

    #[test]
    fn watcher_db_mode_indexes_new_files() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            init_db(&conn).unwrap();
        }
        let opts = options_for(tmp.path());
        let _watcher = WatcherStub::start_with_db(&opts, db_path.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let f = write_file(tmp.path(), "auto.txt", b"auto");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut found = false;
        while std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            if let Ok(entries) = load_db(&db_path) {
                if entries.iter().any(|e| e.path == f) {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "start_with_db must incrementally upsert new files");
    }

    #[test]
    fn poll_thread_reports_last_indexed_and_clears_stale() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(tmp.path(), "p.txt", b"p");
        let db_path = tmp.path().join("poll.db");
        let handle = spawn_poll_thread(
            db_path.clone(),
            options_for(tmp.path()),
            Duration::from_millis(100),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut ok = false;
        while std::time::Instant::now() < deadline {
            if handle.last_indexed().is_some() && !handle.is_stale() {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ok, "daemon must record last_indexed and clear stale");
        let status = handle.status();
        assert!(!status.stale);
        assert!(status.last_indexed.is_some());
        assert!(status.last_indexed_secs().is_some());
        assert_eq!(handle.stale(), handle.is_stale());
        assert!(handle.last_indexed_secs().is_some());
        assert!(db_path.exists());
        handle.stop_and_join();
    }

    #[test]
    fn poll_thread_marks_stale_on_failure() {
        // A db path whose parent can never exist forces every tick to fail.
        let db_path: PathBuf = tmp_missing_db_path();
        let tmp = tempfile::tempdir().unwrap();
        let handle = spawn_poll_thread(db_path, options_for(tmp.path()), Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(400));
        assert!(handle.is_stale(), "failed ticks must set the stale flag");
        assert!(handle.last_indexed().is_none());
        let status = handle.status();
        assert!(status.stale);
        assert!(status.last_indexed.is_none());
        handle.stop_and_join();
    }

    fn tmp_missing_db_path() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "supersearch-no-such-dir-{}",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        base.join("index.db")
    }
}

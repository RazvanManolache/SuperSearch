#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use supersearch_config::{default_config_path, AppConfig, FileGroup, RecentEntry, SavedSearch};
use supersearch_core::export::{self, ExportFormat, ExportHook};
use supersearch_core::FileEntry;
use supersearch_i18n::{fill, I18n, Lang};

/// Phosphor icon glyphs (loaded into the font fallback in `main`).
use egui_phosphor::regular as icons;

/// Warm amber accent for primary actions and section glyphs. Reads on
/// both dark and light themes without the neon-AI purple tell.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(206, 141, 32);
use supersearch_index::{
    entry_is_hidden, fast_search, load_db, spawn_index_thread, PollHandle, ScanOptions,
};
use supersearch_query::{decode_text_bytes, regex_is_valid, search, Query};

/// System font files (by absolute path) appended as proportional
/// fallbacks for scripts the bundled fonts lack — CJK, Thai, Arabic,
/// Hebrew, Indic, Ethiopic, and friends. Best-effort: missing files
/// are skipped, never fatal. Later entries win only for glyphs the
/// earlier fonts lack, so Latin rendering never changes.
#[cfg(windows)]
const SYSTEM_FALLBACK_FONTS: &[&str] = &[
    "C:\\Windows\\Fonts\\segoeui.ttf",
    "C:\\Windows\\Fonts\\tahoma.ttf",
    "C:\\Windows\\Fonts\\LeelawUI.ttf",
    "C:\\Windows\\Fonts\\KhmerUI.ttf",
    "C:\\Windows\\Fonts\\LaoUI.ttf",
    "C:\\Windows\\Fonts\\mmrtext.ttf",
    "C:\\Windows\\Fonts\\mangal.ttf",
    "C:\\Windows\\Fonts\\vrinda.ttf",
    "C:\\Windows\\Fonts\\Latha.ttf",
    "C:\\Windows\\Fonts\\Gautami.ttf",
    "C:\\Windows\\Fonts\\Tunga.ttf",
    "C:\\Windows\\Fonts\\kartika.ttf",
    "C:\\Windows\\Fonts\\raavi.ttf",
    "C:\\Windows\\Fonts\\Shruti.ttf",
    "C:\\Windows\\Fonts\\kalinga.ttf",
    "C:\\Windows\\Fonts\\iskpota.ttf",
    "C:\\Windows\\Fonts\\sylfaen.ttf",
    "C:\\Windows\\Fonts\\ebrima.ttf",
    "C:\\Windows\\Fonts\\malgun.ttf",
    "C:\\Windows\\Fonts\\himalaya.ttf",
    "C:\\Windows\\Fonts\\mvboli.ttf",
    "C:\\Windows\\Fonts\\YuGothR.ttc",
    "C:\\Windows\\Fonts\\msyh.ttc",
    "C:\\Windows\\Fonts\\mingliu.ttc",
];
#[cfg(not(windows))]
const SYSTEM_FALLBACK_FONTS: &[&str] = &[
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
];

/// Load [`SYSTEM_FALLBACK_FONTS`] into `fonts`, appended after the
/// bundled families so they only cover missing glyphs.
fn install_system_fallbacks(fonts: &mut egui::FontDefinitions) {
    let family = fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default();
    for (i, path) in SYSTEM_FALLBACK_FONTS.iter().enumerate() {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let name = format!("sysfb{i}");
        fonts.font_data.insert(
            name.clone(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes)),
        );
        family.push(name);
    }
}

/// Tick interval for the GUI-maintained background indexer (matches the
/// CLI daemon default).
const DAEMON_INTERVAL: Duration = Duration::from_secs(5);
/// How often the indexer status line refreshes while running.
const DAEMON_STATUS_EVERY: Duration = Duration::from_secs(1);
/// Idle delay before a filter edit triggers a live search.
const LIVE_DEBOUNCE: Duration = Duration::from_millis(600);
/// Match highlight colors for preview and filename cells.
const HIGHLIGHT_BG: egui::Color32 = egui::Color32::from_rgb(250, 220, 90);
const HIGHLIGHT_FG: egui::Color32 = egui::Color32::BLACK;

/// Messages from the background scan/search worker thread.
enum WorkerMsg {
    Progress(u64),
    Done(Vec<FileEntry>, String),
    Cancelled,
}

/// Messages from a background export thread: rows written so far, then
/// the final status line (success) or failure/cancel message.
enum ExportMsg {
    Progress(usize),
    Done(Result<String, String>),
}

/// Split a `;`-separated folder field into roots, trimming empties.
/// (`;` is near-never in real paths; commas stay untouched because
/// extensions and exclusions use them.)
fn parse_roots(s: &str) -> Vec<PathBuf> {
    s.split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Same root spelled differently (`C:\` vs `c:\`, trailing slash):
/// platform-aware equality for the chip/drive toggles.
fn roots_equal(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim_end_matches(['/', '\\']).to_string();
    #[cfg(windows)]
    {
        norm(a).eq_ignore_ascii_case(&norm(b))
    }
    #[cfg(not(windows))]
    {
        norm(a) == norm(b)
    }
}

/// Append `root` to the `;`-separated field unless it (or an equal
/// spelling) is already there; empty entries are dropped.
fn add_root(field: &str, root: &str) -> String {
    let mut roots: Vec<String> = parse_roots(field)
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    if !roots.iter().any(|r| roots_equal(r, root)) {
        roots.push(root.trim().to_string());
    }
    roots.join("; ")
}

/// Drop every entry equal to `root` from the `;`-separated field.
fn remove_root(field: &str, root: &str) -> String {
    parse_roots(field)
        .iter()
        .map(|p| p.display().to_string())
        .filter(|r| !roots_equal(r, root))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Toggle `root` in the `;`-separated field (drive buttons).
fn toggle_root(field: &str, root: &str) -> String {
    if parse_roots(field)
        .iter()
        .any(|p| roots_equal(&p.display().to_string(), root))
    {
        remove_root(field, root)
    } else {
        add_root(field, root)
    }
}

/// Quick-select drives: every present `X:\` on Windows; `/` plus the
/// existing top-level mounts (`/home`, `/mnt/*`, `/media/*`) on Unix.
/// Computed once at startup (plugging a drive in later needs a
/// restart to list it).
fn list_drives() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        ('A'..='Z')
            .map(|c| PathBuf::from(format!("{c}:\\")))
            .filter(|p| p.exists())
            .collect()
    }
    #[cfg(not(windows))]
    {
        let mut out = vec![PathBuf::from("/")];
        for base in ["/home", "/mnt", "/media"] {
            let dir = PathBuf::from(base);
            if !dir.is_dir() {
                continue;
            }
            if out
                .iter()
                .all(|p| !roots_equal(&p.display().to_string(), base))
            {
                out.push(dir.clone());
            }
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() && out.len() < 12 {
                        out.push(path);
                    }
                }
            }
        }
        out
    }
}

/// Parse a size field: plain bytes, or a `KB`/`MB`/`GB`/`TB` suffix
/// (case-insensitive, optional space, decimals allowed).
/// Returns the byte count, or `None` for an empty field.
fn parse_size(s: &str, flag: &str, err_template: &str) -> Result<Option<u64>, String> {
    let err = || fill(err_template, &[("flag", flag), ("got", &format!("{s:?}"))]);
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let lower = t.to_ascii_lowercase();
    let (num, mult) = [
        ("tb", 1024u64.pow(4)),
        ("gb", 1024u64.pow(3)),
        ("mb", 1024u64.pow(2)),
        ("kb", 1024u64),
        ("t", 1024u64.pow(4)),
        ("g", 1024u64.pow(3)),
        ("m", 1024u64.pow(2)),
        ("k", 1024u64),
        ("b", 1),
    ]
    .iter()
    .find_map(|(suffix, mult)| {
        lower
            .strip_suffix(suffix)
            .map(|num| (num.trim_end(), *mult))
    })
    .unwrap_or((t, 1));
    if num.is_empty() {
        return Err(err());
    }
    let value: f64 = num.parse().map_err(|_| err())?;
    if !value.is_finite() || value < 0.0 {
        return Err(err());
    }
    Ok(Some((value * mult as f64).round() as u64))
}

/// Right-click row menu choice, applied after the results table is
/// drawn (the menu closure only records; borrows resolve afterwards).
#[derive(Clone, Copy)]
enum RowAction {
    Open,
    Reveal,
    Props,
}

/// Sort identically for worker results and header clicks.
fn sort_entries(entries: &mut [FileEntry], key: SortKey, asc: bool) {
    match key {
        SortKey::Path => entries.sort_by(|a, b| a.path.cmp(&b.path)),
        SortKey::Size => entries.sort_by_key(|e| e.size),
        SortKey::Modified => entries.sort_by_key(|e| e.mtime),
    }
    if !asc {
        entries.reverse();
    }
}

/// Accent-tinted section heading: icon glyph plus strong title.
fn section_heading(ui: &mut egui::Ui, icon: &str, title: String) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(icon).color(ACCENT).size(16.0));
        ui.label(egui::RichText::new(title).strong().size(14.0));
    });
    ui.separator();
}

/// File-type glyph for a result row: folder, then image/audio/video/
/// archive/code/text by extension, plain file otherwise.
fn file_icon(entry: &FileEntry) -> &'static str {
    if entry.is_dir {
        return icons::FOLDER;
    }
    let ext = entry
        .path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") | Some("jpg") | Some("jpeg") | Some("gif") | Some("bmp") | Some("tif")
        | Some("tiff") | Some("webp") | Some("svg") | Some("ico") | Some("heic") => {
            icons::FILE_IMAGE
        }
        Some("mp3") | Some("wav") | Some("flac") | Some("aac") | Some("ogg") | Some("oga")
        | Some("m4a") | Some("wma") | Some("opus") | Some("mid") | Some("midi") => {
            icons::FILE_AUDIO
        }
        // Note: `ts` lives with the code types (TypeScript beats MPEG-TS
        // in a file finder).
        Some("mp4") | Some("mkv") | Some("avi") | Some("mov") | Some("wmv") | Some("flv")
        | Some("webm") | Some("m4v") | Some("mpg") | Some("mpeg") => icons::FILE_VIDEO,
        Some("zip") | Some("rar") | Some("7z") | Some("tar") | Some("gz") | Some("bz2")
        | Some("xz") | Some("cab") | Some("iso") => icons::FILE_ZIP,
        Some("rs") | Some("py") | Some("js") | Some("ts") | Some("jsx") | Some("tsx")
        | Some("c") | Some("h") | Some("cpp") | Some("hpp") | Some("cs") | Some("java")
        | Some("go") | Some("rb") | Some("php") | Some("swift") | Some("kt") | Some("html")
        | Some("css") | Some("json") | Some("xml") | Some("yaml") | Some("yml") | Some("toml")
        | Some("ini") | Some("sql") | Some("sh") | Some("ps1") | Some("bat") | Some("lua") => {
            icons::FILE_CODE
        }
        Some("txt") | Some("md") | Some("csv") | Some("tsv") | Some("rtf") | Some("tex") => {
            icons::FILE_TEXT
        }
        _ => icons::FILE,
    }
}

/// Sortable result-table column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Sortable result-table column. Directories sort with the path
/// column; a separate dir sort burned a whole column on near-zero
/// signal, so it was dropped.
enum SortKey {
    Path,
    Size,
    Modified,
}

/// Page-size choices for the paginated result table.
const PAGE_SIZES: [usize; 4] = [50, 100, 200, 500];
/// Default rows per page; small enough that a 100k+ hit list stays fluid.
const DEFAULT_PAGE_SIZE: usize = 100;

/// One open search tab: its own folders, filters, results, selection,
/// and worker. Tabs never share state except through the global [`App`]
/// (config, indexer, theme, panels, export slot).
struct SearchTab {
    /// `;`-separated folders scanned by this tab.
    root: String,
    use_indexed: bool,
    /// Live-as-you-type for this tab (off by default: searches run on
    /// explicit Search/Enter); the toolbar checkbox mirrors the active
    /// tab.
    live: bool,
    name: String,
    regex: String,
    fuzzy: String,
    content: String,
    glob: String,
    /// Comma-separated extension allow-list, e.g. `rs, md`.
    extensions: String,
    case_insensitive: bool,
    /// Include hidden/system files (off by default, like the CLI).
    show_hidden: bool,
    min_size: String,
    max_size: String,
    modified_after: String,
    modified_before: String,
    /// Comma-separated exclusion patterns (same semantics as
    /// `Query::exclude`).
    exclusions: String,
    /// Comma-separated name substrings: match if ANY hits (OR group).
    or_terms: String,
    /// Comma-separated name substrings: match if NONE hits (NOT group).
    not_terms: String,
    /// Name under which the current filters are saved.
    save_name: String,
    /// Name of the saved search selected in the combo box.
    saved_pick: String,
    /// File group that last filled the extension field (`None` once the
    /// user edits it by hand); drives the chip highlight.
    group_pick: Option<String>,
    status: String,
    scanned: u64,
    scanning: bool,
    results: Vec<FileEntry>,
    rx: Option<Receiver<WorkerMsg>>,
    /// Cooperative cancel flag shared with the worker thread.
    stop_flag: Option<Arc<AtomicBool>>,
    /// Last seen filter key; a change restarts the debounce timer.
    filter_key_cache: String,
    /// When the filters last changed (`None` = no pending live search).
    last_filter_change: Option<Instant>,
    /// Filter key at the last search spawn; differs from the current
    /// key while the filters are dirty (tab dirty dot).
    last_search_key: String,
    /// This tab completed at least one search (empty-state wording).
    searched: bool,
    /// Hit count of the last completed run (survives new scans).
    last_hits: usize,
    /// Selected path to restore after a refresh (page follows it).
    restore_select: Option<PathBuf>,
    /// Bulk-selected result paths. Keyed by path (not position) so
    /// sorting, paging, and follow-up searches keep the selection on
    /// the same files; entries missing from new results simply match
    /// nothing until re-selected.
    checked: HashSet<PathBuf>,
    /// Delete-confirmation modal is open.
    confirm_delete: bool,
    /// A confirm modal opened this frame: focus Cancel once so the
    /// safe choice is the default (and further frames leave focus
    /// alone so Tab still works).
    modal_focus_pending: bool,
    /// Pending move destination picked before the confirm modal.
    pending_move: Option<PathBuf>,
    /// Move-confirmation modal is open.
    confirm_move: bool,
    /// Content needle + case flag the cached preview was built from;
    /// part of the snippet cache key next to the path.
    preview_needle: String,
    preview_ci: bool,
    sort_key: SortKey,
    sort_asc: bool,
    page: usize,
    /// Index into `results` of the row shown in the preview pane.
    selected: Option<usize>,
    /// Cached preview snippet so the file is re-read only when the
    /// selection changes.
    preview_path: PathBuf,
    preview_text: String,
}

impl SearchTab {
    /// Fingerprint of every filter control plus the source selectors.
    /// A folder/DB/mode flip means the old rows answer a different
    /// question, so live search re-runs for those too (debounced like
    /// any other edit).
    fn filter_key(&self, db_path: &str) -> String {
        [
            self.root.as_str(),
            db_path,
            &self.name,
            &self.regex,
            &self.fuzzy,
            &self.content,
            &self.glob,
            &self.extensions,
            &self.min_size,
            &self.max_size,
            &self.modified_after,
            &self.modified_before,
            &self.exclusions,
            &self.or_terms,
            &self.not_terms,
        ]
        .join("\0")
            + if self.case_insensitive { "\0i" } else { "" }
            + if self.use_indexed { "\0db" } else { "" }
            + if self.show_hidden { "\0h" } else { "" }
    }
}

impl Default for SearchTab {
    fn default() -> Self {
        Self {
            root: String::new(),
            use_indexed: false,
            live: false,
            name: String::new(),
            regex: String::new(),
            fuzzy: String::new(),
            content: String::new(),
            glob: String::new(),
            extensions: String::new(),
            case_insensitive: false,
            show_hidden: false,
            min_size: String::new(),
            max_size: String::new(),
            modified_after: String::new(),
            modified_before: String::new(),
            exclusions: String::new(),
            or_terms: String::new(),
            not_terms: String::new(),
            save_name: String::new(),
            saved_pick: String::new(),
            group_pick: None,
            status: String::new(),
            scanned: 0,
            scanning: false,
            results: Vec::new(),
            rx: None,
            stop_flag: None,
            filter_key_cache: String::new(),
            last_filter_change: None,
            last_search_key: String::new(),
            searched: false,
            last_hits: 0,
            restore_select: None,
            checked: HashSet::new(),
            confirm_delete: false,
            modal_focus_pending: false,
            pending_move: None,
            confirm_move: false,
            preview_needle: String::new(),
            preview_ci: false,
            sort_key: SortKey::Path,
            sort_asc: true,
            page: 0,
            selected: None,
            preview_path: PathBuf::new(),
            preview_text: String::new(),
        }
    }
}

struct App {
    db_path: String,
    /// Background indexer handle (`spawn_index_thread`); `None` while
    /// stopped. The thread owns its DB connections, so the UI thread
    /// only polls `last_indexed`/`is_stale` — never blocks on it.
    daemon: Option<PollHandle>,
    /// Last indexer status line shown under the Start/Stop button.
    daemon_status: String,
    /// Last time the indexer status line was recomputed (1 Hz gate so
    /// the "Ns ago" readout updates without repainting every frame).
    daemon_poll: Option<Instant>,
    page_size: usize,
    dark_mode: bool,
    /// Last print-view temp file: removed on the next print and on
    /// exit so print views never accumulate in the temp dir.
    last_print: Option<PathBuf>,
    /// CSV field delimiter used by the Export CSV button (`,` default;
    /// `;`, tab, `|` for locales needing them).
    csv_delim: char,
    /// Filter sidebar and preview pane visibility (toolbar toggles).
    show_filters: bool,
    show_preview: bool,
    /// Background export channel (`None` while idle); the write runs on
    /// its own thread so even million-row exports never freeze the UI.
    export_rx: Option<Receiver<ExportMsg>>,
    /// Rows the running export claims, and rows written so far.
    export_total: usize,
    export_rows: usize,
    /// Cooperative cancel flag shared with the export thread.
    export_cancel: Option<Arc<AtomicBool>>,
    /// Custom file-group manager modal is open.
    manage_groups: bool,
    /// Help modal is open.
    show_help: bool,
    /// System integration modal is open.
    show_integration: bool,
    /// Display (text size) modal is open.
    show_display: bool,
    /// Active UI language (toolbar switcher mirrors this).
    lang: Lang,
    /// Keyed UI strings for [`App::lang`] (English fallback built in).
    tr: I18n,
    /// Language picker modal is open.
    lang_pick_open: bool,
    /// Filter text inside the language picker.
    lang_filter: String,
    /// Draft name/extensions for the group manager's add form.
    new_group_name: String,
    new_group_exts: String,
    /// Quick-select drives, computed once at startup.
    drives: Vec<PathBuf>,
    /// Open searches; `active` indexes the visible one.
    tabs: Vec<SearchTab>,
    active: usize,
    config: AppConfig,
    config_path: PathBuf,
}

impl Default for App {
    fn default() -> Self {
        Self::new_with_config(
            AppConfig::default(),
            std::env::temp_dir().join("ss-test-no-write.json"),
        )
    }
}

/// Route bare `self.<field>` access to the ACTIVE tab: every method
/// written against the old flat `App` keeps working unchanged, while
/// new tab-aware code uses `tab()`/`tab_mut()` or indexes `tabs`
/// directly. App-level fields (`db_path`, `config`, `page_size`, …)
/// resolve first, so there is no ambiguity.
impl std::ops::Deref for App {
    type Target = SearchTab;
    fn deref(&self) -> &SearchTab {
        &self.tabs[self.active]
    }
}

impl std::ops::DerefMut for App {
    fn deref_mut(&mut self) -> &mut SearchTab {
        &mut self.tabs[self.active]
    }
}

impl App {
    /// Load the persisted config and pre-fill the controls with it.
    fn new() -> Self {
        let config_path = default_config_path();
        let config = AppConfig::load_from(&config_path).unwrap_or_default();
        Self::new_with_config(config, config_path)
    }

    /// Build from an explicit config (production passes the loaded
    /// one; tests pass defaults). Hermetic by construction: no disk
    /// read here, so unit tests never inherit the ambient user
    /// config (language, roots, DB path) from the machine they run
    /// on.
    fn new_with_config(config: AppConfig, config_path: PathBuf) -> Self {
        // All remembered roots, `;`-joined: multi-root CLI configs
        // open intact instead of being clobbered to the first root.
        let root = config
            .roots
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("; ");
        let db_path = config
            .db_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let exclusions = config.exclusions.join(", ");
        // Remembered delimiter, validated: anything outside the
        // picker set falls back to comma.
        let csv_delim = match config.csv_delim {
            ',' | ';' | '\t' | '|' => config.csv_delim,
            _ => ',',
        };
        // Remembered language, validated: unknown codes fall back to
        // English (same rule as `Lang::from_code`).
        let lang = Lang::from_code(&config.language);
        // First tab owns the remembered roots (`;`-joined so multi-root
        // CLI configs survive the GUI, fixing silent clobbering).
        let mut first = SearchTab::default();
        first.root = root;
        first.exclusions = exclusions;
        let mut app = Self {
            db_path,
            daemon: None,
            daemon_status: String::new(),
            daemon_poll: None,
            page_size: DEFAULT_PAGE_SIZE,
            dark_mode: true,
            last_print: None,
            csv_delim,
            show_filters: true,
            show_preview: true,
            export_rx: None,
            export_total: 0,
            export_rows: 0,
            export_cancel: None,
            manage_groups: false,
            show_help: false,
            show_integration: false,
            show_display: false,
            lang,
            tr: I18n::load(lang),
            lang_pick_open: false,
            lang_filter: String::new(),
            new_group_name: String::new(),
            new_group_exts: String::new(),
            drives: list_drives(),
            tabs: vec![first],
            active: 0,
            config,
            config_path,
        };
        // Seed the live-search key so launch itself never counts as a
        // filter change (no surprise scan 600 ms after startup).
        let key = app.filter_key();
        app.tab_mut().filter_key_cache = key;
        app
    }

    /// The visible tab.
    fn tab(&self) -> &SearchTab {
        &self.tabs[self.active]
    }

    /// The visible tab, mutably.
    fn tab_mut(&mut self) -> &mut SearchTab {
        &mut self.tabs[self.active]
    }

    /// Open a new tab inheriting the active tab's folders and Live
    /// mode, then switch to it.
    fn new_tab(&mut self) {
        let mut tab = SearchTab::default();
        tab.root = self.tab().root.clone();
        tab.live = self.tab().live;
        tab.exclusions = self.tab().exclusions.clone();
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    /// Close tab `i` (never the last one); a running scan there is
    /// cancelled first so no worker outlives its tab.
    fn close_tab(&mut self, i: usize) {
        if self.tabs.len() <= 1 || i >= self.tabs.len() {
            return;
        }
        if let Some(stop) = self.tabs[i].stop_flag.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.tabs.remove(i);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if i < self.active {
            self.active -= 1;
        }
    }

    /// Cancel the worker running on tab `i`, if any.
    fn cancel_tab(&mut self, i: usize) {
        if let Some(tab) = self.tabs.get_mut(i) {
            if let Some(stop) = tab.stop_flag.take() {
                stop.store(true, Ordering::Relaxed);
            }
            tab.rx = None;
            tab.scanning = false;
        }
    }

    /// Short tab title: first root's basename, or "Search N".
    /// Short tab title: what is searched and where, e.g.
    /// `report · docs +1`. Blank tabs keep the numbered fallback.
    fn tab_title(&self, i: usize) -> String {
        let tab = &self.tabs[i];
        let mut what = tab.name.trim();
        if what.is_empty() {
            what = tab.glob.trim();
        }
        if what.is_empty() {
            what = tab.regex.trim();
        }
        if what.is_empty() {
            what = tab.fuzzy.trim();
        }
        if what.is_empty() {
            what = tab.content.trim();
        }
        if what.is_empty() && !tab.or_terms.trim().is_empty() {
            what = tab.or_terms.trim();
        }
        let is_all = what.is_empty();
        let what: String = if is_all {
            self.tr.t("tab.all").to_string()
        } else {
            let mut s: String = what.chars().take(20).collect();
            if what.chars().count() > 20 {
                s.push('…');
            }
            s
        };
        let roots: Vec<&str> = tab
            .root
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        let where_ = match roots.as_slice() {
            [] => self.tr.t("tab.nofolder").to_string(),
            [first] => PathBuf::from(first)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| first.to_string()),
            [first, rest @ ..] => {
                let base = PathBuf::from(first)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| first.to_string());
                format!("{base} +{}", rest.len())
            }
        };
        if is_all && roots.is_empty() {
            self.tr.f("tab.blank", &[("n", &(i + 1).to_string())])
        } else {
            format!("{what} · {where_}")
        }
    }

    /// Write the current control state back into the config and save it.
    /// Failures warn in the status line but never break the UI.
    fn persist(&mut self) {
        // Multi-root: the folder box may hold several `;`-separated
        // roots; every existing one is remembered for next launch.
        let roots: Vec<PathBuf> = parse_roots(&self.root)
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        let db = {
            let t = self.db_path.trim();
            if t.is_empty() {
                None
            } else {
                Some(PathBuf::from(t))
            }
        };
        self.config.record_usage(&roots, db.as_ref());
        self.config.exclusions = parse_exclusions(&self.exclusions);
        self.config.csv_delim = self.csv_delim;
        self.config.language = self.lang.code().to_string();
        if let Err(e) = self.config.save_to(&self.config_path) {
            self.status = self.tr.f(
                "status.config_save_fail",
                &[
                    ("path", &self.config_path.display().to_string()),
                    ("e", &e.to_string()),
                ],
            );
        }
    }

    /// Point the first tab at `dir` (context-menu / launcher handoff):
    /// replaces the remembered root and reseeds the live-search key so
    /// launch never counts as a filter change.
    fn set_initial_root(&mut self, dir: PathBuf) {
        self.tabs[0].root = dir.display().to_string();
        let key = self.filter_key();
        self.tabs[0].filter_key_cache = key;
    }

    /// Switch the UI language now and remember it: rebuilds the string
    /// table and persists, so the next launch opens translated.
    fn set_language(&mut self, lang: Lang) {
        self.lang = lang;
        self.tr = I18n::load(lang);
        self.config.language = lang.code().to_string();
        if let Err(e) = self.config.save_to(&self.config_path) {
            self.status = self.tr.f(
                "status.config_save_fail",
                &[
                    ("path", &self.config_path.display().to_string()),
                    ("e", &e.to_string()),
                ],
            );
        }
    }

    /// Registry subkeys (HKCU: no admin needed) for the Explorer
    /// "Search with SuperSearch" verbs — one for a folder's own
    /// right-click, one for the background inside a folder. Both run
    /// this exe with `--folder "%V"`.
    #[cfg(windows)]
    const CTX_FOLDER_KEY: &'static str = r"Software\Classes\Directory\shell\SuperSearch";
    #[cfg(windows)]
    const CTX_BG_KEY: &'static str = r"Software\Classes\Directory\Background\shell\SuperSearch";

    /// Whether the Explorer context menu entries are currently
    /// installed (Windows), or any file-manager action is (Linux).
    #[cfg(windows)]
    fn context_menu_installed() -> bool {
        use winreg::{enums::HKEY_CURRENT_USER, RegKey};
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        [Self::CTX_FOLDER_KEY, Self::CTX_BG_KEY]
            .iter()
            .all(|base| hkcu.open_subkey(format!("{base}\\command")).is_ok())
    }

    /// Install (`on`) or remove the Explorer context menu entries.
    /// Errors are plain English: the caller renders them through
    /// `integration.fail`.
    #[cfg(windows)]
    fn set_context_menu(on: bool) -> Result<(), String> {
        use winreg::{enums::HKEY_CURRENT_USER, RegKey};
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if !on {
            for base in [Self::CTX_FOLDER_KEY, Self::CTX_BG_KEY] {
                let _ = hkcu.delete_subkey_all(base);
            }
            return Ok(());
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let cmd = format!("\"{}\" --folder \"%V\"", exe.display());
        for base in [Self::CTX_FOLDER_KEY, Self::CTX_BG_KEY] {
            let (key, _) = hkcu.create_subkey(base).map_err(|e| e.to_string())?;
            key.set_value("", &"Search with SuperSearch")
                .map_err(|e| e.to_string())?;
            key.set_value("Icon", &format!("{},0", exe.display()))
                .map_err(|e| e.to_string())?;
            let (cmd_key, _) = hkcu
                .create_subkey(format!("{base}\\command"))
                .map_err(|e| e.to_string())?;
            cmd_key.set_value("", &cmd).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// User-level file-manager actions installed by
    /// [`Self::set_context_menu`] on Linux: Nautilus script, Dolphin
    /// service menu (KF5 + KF6 paths), Nemo action.
    #[cfg(not(windows))]
    fn linux_action_files() -> Vec<PathBuf> {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        [
            ".local/share/nautilus/scripts/Search with SuperSearch",
            ".local/share/kio/servicemenus/supersearch.desktop",
            ".local/share/kservices5/ServiceMenus/supersearch.desktop",
            ".local/share/nemo/actions/supersearch.nemo_action",
        ]
        .iter()
        .map(|rel| PathBuf::from(&home).join(rel))
        .collect()
    }

    #[cfg(not(windows))]
    const NAUTILUS_SCRIPT: &'static str = r#"#!/bin/sh
# Search the selected folders with SuperSearch (installed from its
# System integration settings).
for p in "$@"; do
    case "$p" in file://*) p=$(printf '%s' "$p" | sed 's|^file://||');; esac
    if [ -d "$p" ]; then exec supersearch-gui --folder "$p"; fi
done
uri=${NAUTILUS_SCRIPT_CURRENT_URI#file://}
exec supersearch-gui --folder "${uri:-$HOME}"
"#;

    #[cfg(not(windows))]
    const DOLPHIN_MENU: &'static str = r#"[Desktop Entry]
Type=Service
Actions=searchHere;
X-KDE-ServiceTypes=KonqPopupMenu/Plugin,inode/directory
[Desktop Action searchHere]
Name=Search with SuperSearch
Exec=supersearch-gui --folder %f
Icon=system-search
"#;

    #[cfg(not(windows))]
    const NEMO_ACTION: &'static str = r#"[Nemo Action]
Name=Search with SuperSearch
Exec=supersearch-gui --folder %F
Selection=any
Extensions=dir;
"#;

    #[cfg(not(windows))]
    fn context_menu_installed() -> bool {
        Self::linux_action_files().iter().any(|p| p.exists())
    }

    #[cfg(not(windows))]
    fn set_context_menu(on: bool) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt as _;
        let files = Self::linux_action_files();
        if !on {
            for p in &files {
                let _ = std::fs::remove_file(p);
            }
            return Ok(());
        }
        let payloads = [
            Self::NAUTILUS_SCRIPT,
            Self::DOLPHIN_MENU,
            Self::DOLPHIN_MENU,
            Self::NEMO_ACTION,
        ];
        for (path, text) in files.iter().zip(payloads) {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(path, text).map_err(|e| e.to_string())?;
        }
        // Nautilus scripts must be executable.
        let mut perm = std::fs::metadata(&files[0])
            .map_err(|e| e.to_string())?
            .permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&files[0], perm).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Snapshot of the current filter fields as a named saved search.
    ///
    /// Goes through the same [`SavedSearch::from_query`] conversion as
    /// everything else, so no filter the controls expose is dropped.
    fn current_saved_search(&self, name: &str) -> Result<SavedSearch, String> {
        // Roots and the hidden flag travel outside the query, so they
        // are stamped on the snapshot after the conversion (OR/NOT
        // terms ride along inside it).
        let mut saved = SavedSearch::from_query(name, &self.current_query()?);
        saved.root = self.root.trim().to_string();
        saved.show_hidden = self.show_hidden;
        Ok(saved)
    }

    /// Copy a saved search into the filter controls.
    fn apply_saved_search(&mut self, saved: &SavedSearch) {
        self.name = saved.name_filter.clone().unwrap_or_default();
        self.regex = saved.regex.clone().unwrap_or_default();
        self.fuzzy = saved.fuzzy.clone().unwrap_or_default();
        self.content = saved.content.clone().unwrap_or_default();
        self.glob = saved.glob.clone().unwrap_or_default();
        self.extensions = saved.extensions.join(", ");
        self.case_insensitive = saved.case_insensitive;
        self.min_size = saved.min_size.map(|n| n.to_string()).unwrap_or_default();
        self.max_size = saved.max_size.map(|n| n.to_string()).unwrap_or_default();
        self.modified_after = saved
            .modified_after
            .map(|n| n.to_string())
            .unwrap_or_default();
        self.modified_before = saved
            .modified_before
            .map(|n| n.to_string())
            .unwrap_or_default();
        self.exclusions = saved.exclude.join(", ");
        // Legacy snapshots predate stored roots (empty): they restore
        // the filters and keep the tab's current folders.
        if !saved.root.trim().is_empty() {
            self.root = saved.root.clone();
        }
        self.or_terms = saved.or_terms.join(", ");
        self.not_terms = saved.not_terms.join(", ");
        self.show_hidden = saved.show_hidden;
        // A restored snapshot is a fresh filter set, not a group pick.
        self.group_pick = None;
    }
}

impl App {
    /// Drain every tab's worker messages. Each tab owns its worker, so
    /// background tabs report progress and land results independently;
    /// the active tab's state drives the status bar and table.
    fn poll_worker(&mut self) {
        let page_size = self.page_size;
        for i in 0..self.tabs.len() {
            let mut progress: Option<u64> = None;
            let mut done: Option<(Vec<FileEntry>, String)> = None;
            let mut cancelled = false;
            // Drain first (borrow ends with the block), mutate after.
            {
                if let Some(rx) = self.tabs[i].rx.as_ref() {
                    while let Ok(msg) = rx.try_recv() {
                        match msg {
                            WorkerMsg::Progress(n) => progress = Some(n),
                            WorkerMsg::Done(entries, status) => {
                                done = Some((entries, status));
                                break;
                            }
                            WorkerMsg::Cancelled => {
                                cancelled = true;
                                break;
                            }
                        }
                    }
                }
            }
            if let Some(n) = progress {
                self.tabs[i].scanned = n;
            }
            let tab = &mut self.tabs[i];
            if let Some((entries, status)) = done {
                // Pre-sorted by the worker: assigning is O(1) and the
                // frame never blocks, even for million-hit runs.
                tab.results = entries;
                tab.searched = true;
                tab.last_hits = tab.results.len();
                tab.preview_path = PathBuf::new();
                tab.preview_text.clear();
                // Drop checks for files the new result set no longer
                // has; survivors keep their selection across refines.
                let present: HashSet<PathBuf> =
                    tab.results.iter().map(|e| e.path.clone()).collect();
                tab.checked.retain(|p| present.contains(p));
                // Refresh keeps the selected file when it survives,
                // and the page follows it; otherwise start at the top.
                if let Some(path) = tab.restore_select.take() {
                    if let Some(pos) = tab.results.iter().position(|e| e.path == path) {
                        tab.selected = Some(pos);
                        tab.page = pos / page_size;
                    } else {
                        tab.selected = None;
                        tab.page = 0;
                    }
                } else {
                    tab.page = 0;
                    tab.selected = None;
                }
                tab.status = status;
                tab.scanning = false;
                tab.rx = None;
                tab.stop_flag = None;
            } else if cancelled {
                let tab = &mut self.tabs[i];
                tab.scanning = false;
                tab.rx = None;
                tab.stop_flag = None;
                tab.status = "Cancelled.".to_string();
            }
        }
    }

    fn build_query(&self) -> Result<Query, String> {
        self.current_query()
    }

    /// The single constructor for the runnable query behind both
    /// searching and saving, so the two can never drift apart. An
    /// invalid regex is an error here (status line), never a silent
    /// empty table.
    fn current_query(&self) -> Result<Query, String> {
        if !self.regex.trim().is_empty() && !regex_is_valid(&self.regex) {
            return Err(self.tr.f(
                "status.bad_regex",
                &[("q", &format!("{:?}", self.regex.trim()))],
            ));
        }
        // OR/NOT terms become single-substring subqueries; anything
        // richer stays a CLI/query-crate affair.
        let ors = parse_exclusions(&self.or_terms)
            .into_iter()
            .map(|term| Query {
                name_substr: Some(term),
                ..Query::default()
            })
            .collect();
        let nots = parse_exclusions(&self.not_terms)
            .into_iter()
            .map(|term| Query {
                name_substr: Some(term),
                ..Query::default()
            })
            .collect();
        Ok(Query {
            name_substr: opt_text(&self.name),
            regex: opt_text(&self.regex),
            fuzzy: opt_text(&self.fuzzy),
            content: opt_text(&self.content),
            glob: opt_text(&self.glob),
            extensions: parse_exclusions(&self.extensions),
            case_insensitive: self.case_insensitive,
            min_size: parse_size(
                &self.min_size,
                &self.tr.t("status.flag_min"),
                &self.tr.t("status.bad_size"),
            )?,
            max_size: parse_size(
                &self.max_size,
                &self.tr.t("status.flag_max"),
                &self.tr.t("status.bad_size"),
            )?,
            modified_after: opt_mtime(&self.modified_after, &self.tr.t("status.flag_after"))?,
            modified_before: opt_mtime(&self.modified_before, &self.tr.t("status.flag_before"))?,
            exclude: parse_exclusions(&self.exclusions),
            or: ors,
            not: nots,
            ..Query::default()
        })
    }

    /// Sort `results` in place by the active column and direction
    /// (header clicks; the worker threads pre-sort initial results
    /// the same way so Done never blocks the frame).
    fn sort_results(&mut self) {
        let (key, asc) = (self.sort_key, self.sort_asc);
        sort_entries(&mut self.results, key, asc);
    }

    /// Switch the sort column (toggling direction on a repeat click),
    /// then re-sort and reset pagination and selection.
    fn set_sort(&mut self, key: SortKey) {
        if self.sort_key == key {
            self.sort_asc = !self.sort_asc;
        } else {
            self.sort_key = key;
            self.sort_asc = true;
        }
        self.sort_results();
        self.page = 0;
        self.selected = None;
    }

    fn sort_arrow(&self, key: SortKey) -> &'static str {
        if self.sort_key != key {
            ""
        } else if self.sort_asc {
            icons::CARET_UP
        } else {
            icons::CARET_DOWN
        }
    }

    /// Reset every filter control to its default. Roots, DB, and mode
    /// are source state, not filters, so they survive.
    fn clear_filters(&mut self) {
        self.name.clear();
        self.regex.clear();
        self.fuzzy.clear();
        self.content.clear();
        self.glob.clear();
        self.extensions.clear();
        self.case_insensitive = false;
        self.min_size.clear();
        self.max_size.clear();
        self.modified_after.clear();
        self.modified_before.clear();
        self.exclusions.clear();
        self.or_terms.clear();
        self.not_terms.clear();
        self.show_hidden = false;
        self.group_pick = None;
        self.status = self.tr.t("status.filters_cleared").to_string();
    }

    fn page_count(&self) -> usize {
        if self.results.is_empty() {
            1
        } else {
            self.results.len().div_ceil(self.page_size)
        }
    }

    fn clamp_page(&mut self) {
        let pages = self.page_count();
        if self.page >= pages {
            self.page = pages - 1;
        }
    }

    /// Cooperative cancel: signal the worker to abandon its results,
    /// detach from it, and report back immediately. The worker thread may
    /// run to completion in the background, but its sends fail once the
    /// channel is dropped, so no stale results can land in the UI.
    fn on_cancel(&mut self) {
        // The active tab's worker, if any; background tabs cancel via
        // their close button, or by switching here first.
        self.cancel_tab(self.active);
        self.status = self.tr.t("status.cancelled").to_string();
    }

    /// Start or stop the background indexer for the current folder/DB
    /// fields. Starting reuses `spawn_index_thread` (delta ticks on
    /// Windows volumes, walk-polling elsewhere); stopping only signals
    /// the flag — the thread exits on its own and never blocks the UI.
    fn on_daemon_toggle(&mut self) {
        if let Some(handle) = self.daemon.take() {
            handle.stop();
            self.daemon_poll = None;
            self.daemon_status = self.tr.t("status.daemon_stopped").to_string();
            return;
        }
        // The active tab's folders, minus missing ones (a stale
        // remembered folder must not block indexing the rest).
        let roots: Vec<PathBuf> = parse_roots(&self.root)
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        if roots.is_empty() {
            self.daemon_status = self.tr.t("status.daemon_folder").to_string();
            return;
        }
        if self.db_path.trim().is_empty() {
            self.daemon_status = self.tr.t("status.daemon_db").to_string();
            return;
        }
        self.persist();
        let options = ScanOptions {
            roots,
            follow_symlinks: false,
            // The index stores hidden rows too; the per-tab Show
            // hidden toggle filters them at search time instead, so
            // one index serves both views.
            skip_hidden: false,
        };
        let db = PathBuf::from(self.db_path.trim());
        self.daemon = Some(spawn_index_thread(db, options, DAEMON_INTERVAL));
        self.daemon_poll = None;
        self.daemon_status = self.tr.t("status.daemon_started").to_string();
    }

    /// Refresh the indexer status line at 1 Hz while running. Only
    /// reads atomics/locks owned by the daemon thread — never blocks.
    /// The handle snapshot is owned (plain data, no borrow held) so
    /// the status fields can update freely below.
    fn poll_daemon_status(&mut self) {
        if self.daemon.is_none() {
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.daemon_poll {
            if now.duration_since(last) < DAEMON_STATUS_EVERY {
                return;
            }
        }
        self.daemon_poll = Some(now);
        let (last_indexed, stale) = self
            .daemon
            .as_ref()
            .map(|handle| (handle.last_indexed(), handle.is_stale()))
            .unwrap_or((None, true));
        let ago = last_indexed
            .and_then(|t| t.elapsed().ok())
            .map(|d| {
                self.tr
                    .f("status.daemon_ago", &[("n", &d.as_secs().to_string())])
            })
            .unwrap_or_else(|| self.tr.t("status.daemon_never").to_string());
        self.daemon_status = self.tr.f(
            "status.daemon_status",
            &[("ago", &ago), ("stale", &stale.to_string())],
        );
    }

    /// Active tab's filter key: compat wrapper so live search, tests,
    /// and callers keep working unchanged.
    fn filter_key(&self) -> String {
        self.tab().filter_key(&self.db_path)
    }

    /// Track filter edits and fire a debounced live search. Called every
    /// frame; cheap string compare, no work when idle.
    fn maybe_live_search(&mut self) {
        let key = self.filter_key();
        if key != self.filter_key_cache {
            self.filter_key_cache = key;
            self.last_filter_change = Some(Instant::now());
        }
        if !self.live || self.scanning {
            return;
        }
        if let Some(changed) = self.last_filter_change {
            if changed.elapsed() >= LIVE_DEBOUNCE {
                self.last_filter_change = None;
                self.on_search();
            }
        }
    }

    /// Move the row selection by `delta` (arrow keys), clamping to the
    /// result list and syncing the page. No-op on an empty table.
    fn step_selection(&mut self, delta: isize) {
        if self.results.is_empty() {
            return;
        }
        let next = match self.selected {
            Some(i) => (i as isize + delta).clamp(0, self.results.len() as isize - 1) as usize,
            None => {
                if delta >= 0 {
                    0
                } else {
                    self.results.len() - 1
                }
            }
        };
        self.selected = Some(next);
        self.page = next / self.page_size;
    }

    /// Toggle the checkbox of the selected row (Space key). The preview
    /// is untouched: checking is not selecting.
    fn toggle_selected_check(&mut self) {
        if let Some(i) = self.selected {
            if let Some(e) = self.results.get(i) {
                let path = e.path.clone();
                if !self.checked.remove(&path) {
                    self.checked.insert(path);
                }
            }
        }
    }

    /// Paths of the bulk-checked rows, in result order. Pure helper so
    /// the selection plumbing stays headless-testable.
    fn checked_paths(&self) -> Vec<PathBuf> {
        self.results
            .iter()
            .filter(|e| self.checked.contains(&e.path))
            .map(|e| e.path.clone())
            .collect()
    }

    /// Write every current hit in `format` to a picked file.
    fn on_export(&mut self, kind: &str, ext: &str, format: ExportFormat) {
        self.on_export_stream(kind, ext, format);
    }

    /// Print path: render HTML to a fresh temp file and open it in
    /// the default browser (print from there). The previous print
    /// file is removed first and the new one on exit, so print views
    /// never accumulate in the temp dir.
    fn on_print(&mut self) {
        if let Some(old) = self.last_print.take() {
            let _ = std::fs::remove_file(old);
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("supersearch-print-{stamp}.html"));
        match std::fs::write(&path, export::to_html(&self.results)) {
            Ok(()) => match open_path(&path) {
                Ok(()) => {
                    self.status = self.tr.f(
                        "status.print_view",
                        &[("path", &path.display().to_string())],
                    );
                    self.last_print = Some(path);
                }
                Err(e) => self.status = e,
            },
            Err(e) => self.status = self.tr.f("status.print_fail", &[("e", &e.to_string())]),
        }
    }

    /// Start a search on a worker thread (both live-scan and indexed-DB
    /// paths), reporting progress back to the UI thread.
    fn on_search(&mut self) {
        // New search, same selection where paths survive (path-keyed
        // checks ride out sorts, pages, and refines). Only the dialogs
        // reset: their pending targets belong to the old run.
        self.confirm_delete = false;
        self.confirm_move = false;
        self.pending_move = None;
        let query = match self.build_query() {
            Ok(q) => q,
            Err(e) => {
                self.status = self.tr.f("status.bad_filter", &[("e", &e)]);
                return;
            }
        };
        // An empty filter set matches the whole volume: refuse to run
        // it, live or explicit. `*` in Glob says what it means.
        if query == Query::default() {
            self.status = self.tr.t("status.need_filter").to_string();
            return;
        }
        // Remember this run: roots/db for next launch, the full filter
        // set as a recent snapshot (clicking it restores every filter),
        // then persist (best effort) so state survives exit.
        // Match-everything runs are not remembered.
        if query != Query::default() {
            // Roots and the hidden flag travel outside the query, so
            // they are stamped on the snapshot after the conversion.
            let mut snap = SavedSearch::from_query(self.name.trim(), &query);
            snap.root = self.root.trim().to_string();
            snap.show_hidden = self.show_hidden;
            self.config.push_recent_search(snap);
        }
        self.persist();
        // Snapshot everything the worker needs while the borrows are
        // still shared; the tab is mutably borrowed only below.
        let use_indexed = self.use_indexed;
        let show_hidden = self.show_hidden;
        let sort_spec = (self.sort_key, self.sort_asc);
        let db_path = self.db_path.trim().to_string();
        let search_key = self.filter_key();
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let tab = self.tab_mut();
        tab.stop_flag = Some(Arc::clone(&stop));
        // `last_search_key` is stamped at each spawn below (not here):
        // validation failures above must leave the dirty dot intact.
        // Sort spec travels with the scan: workers deliver Done
        // pre-sorted so the UI thread never sorts (a 6M-hit sort
        // froze the frame into "Not Responding").
        tab.rx = Some(rx);
        tab.scanning = true;
        tab.scanned = 0;
        tab.page = 0;
        // Old rows stay visible with a Scanning/Loading status until the
        // worker delivers (or cancelling keeps them too); only a
        // successful Done replaces the table.
        if use_indexed {
            // Indexed path also runs off the UI thread so a large DB
            // never freezes the window; progress is coarse (loading,
            // then row count) because SQLite reads have no stride.
            if db_path.is_empty() {
                self.status = self.tr.t("status.enter_db").to_string();
                self.scanning = false;
                self.rx = None;
                self.stop_flag = None;
                return;
            }
            let db_file = PathBuf::from(&db_path);
            if !db_file.exists() {
                self.status = self.tr.f(
                    "status.index_missing",
                    &[("db", &db_file.display().to_string())],
                );
                self.scanning = false;
                self.rx = None;
                self.stop_flag = None;
                return;
            }
            self.status = self.tr.f("status.loading_index", &[("db", &db_path)]);
            self.tab_mut().last_search_key = search_key;
            // Templates owned into the worker: no borrow crosses the
            // thread boundary.
            let index_failed_tmpl = self.tr.t("status.index_failed").to_string();
            let index_hits_tmpl = self.tr.t("status.index_hits").to_string();
            std::thread::spawn(move || {
                if stop.load(Ordering::Relaxed) {
                    let _ = tx.send(WorkerMsg::Cancelled);
                    return;
                }
                let _ = tx.send(WorkerMsg::Progress(0));
                let entries = match load_db(&db_file) {
                    Ok(entries) => entries,
                    Err(e) => {
                        let msg = fill(
                            &index_failed_tmpl,
                            &[
                                ("db", &db_file.display().to_string()),
                                ("e", &e.to_string()),
                            ],
                        );
                        let _ = tx.send(WorkerMsg::Done(Vec::new(), msg));
                        return;
                    }
                };
                if stop.load(Ordering::Relaxed) {
                    let _ = tx.send(WorkerMsg::Cancelled);
                    return;
                }
                let _ = tx.send(WorkerMsg::Progress(entries.len() as u64));
                let total = entries.len();
                let hits = search(&entries, &query);
                if stop.load(Ordering::Relaxed) {
                    let _ = tx.send(WorkerMsg::Cancelled);
                    return;
                }
                let mut owned: Vec<FileEntry> = hits.into_iter().cloned().collect();
                // The index keeps hidden rows (the daemon stores
                // everything); the per-tab toggle filters them here so
                // indexed search honors it exactly like live search.
                // Unstatable paths are kept: a stale row still answers
                // the query, and pruning it is the indexer's job.
                if !show_hidden {
                    owned.retain(|e| !entry_hidden(&e.path));
                }
                let status = fill(
                    &index_hits_tmpl,
                    &[
                        ("n", &owned.len().to_string()),
                        ("m", &total.to_string()),
                        ("db", &db_file.display().to_string()),
                    ],
                );
                sort_entries(&mut owned, sort_spec.0, sort_spec.1);
                let _ = tx.send(WorkerMsg::Done(owned, status));
            });
            return;
        }
        // Several `;`-separated roots search at once; missing ones
        // warn instead of failing so one stale remembered folder never
        // blocks the rest.
        let mut valid: Vec<PathBuf> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        for root in parse_roots(&self.root) {
            if root.exists() {
                valid.push(root);
            } else {
                missing.push(root.display().to_string());
            }
        }
        if valid.is_empty() {
            self.status = if missing.is_empty() {
                self.tr.t("status.pick_folder").to_string()
            } else {
                self.tr
                    .f("status.no_folders", &[("list", &missing.join("; "))])
            };
            self.scanning = false;
            self.rx = None;
            self.stop_flag = None;
            return;
        }
        // One folder reads "under X"; several read "across N folders"
        // so the status line stays short no matter how many roots.
        let scope = if valid.len() == 1 {
            self.tr.f(
                "status.scope_one",
                &[("root", &valid[0].display().to_string())],
            )
        } else {
            self.tr
                .f("status.scope_multi", &[("n", &valid.len().to_string())])
        };
        self.status = self.tr.t("status.scanning").to_string();
        self.tab_mut().last_search_key = search_key;
        // Templates owned into the worker: no borrow crosses the
        // thread boundary.
        let live_hits_tmpl = self.tr.t("status.live_hits").to_string();
        let missing_tmpl = self.tr.t("status.missing").to_string();
        let skipped_tmpl = self.tr.t("status.skipped").to_string();
        std::thread::spawn(move || {
            let options = ScanOptions {
                roots: valid,
                follow_symlinks: false,
                skip_hidden: !show_hidden,
            };
            let progress_tx = tx.clone();
            let progress = move |n: u64| {
                let _ = progress_tx.send(WorkerMsg::Progress(n));
            };
            // MFT name table on elevated Windows, tuned walk elsewhere.
            // The table shows size/mtime, so metadata is always wanted.
            let outcome = fast_search(&options, &query, true, Some(&progress));
            if stop.load(Ordering::Relaxed) {
                let _ = tx.send(WorkerMsg::Cancelled);
                return;
            }
            let mut status = fill(
                &live_hits_tmpl,
                &[
                    ("n", &outcome.entries.len().to_string()),
                    ("m", &outcome.scanned.to_string()),
                    ("scope", &scope),
                    ("via", &outcome.note),
                ],
            );
            if !missing.is_empty() {
                status.push_str(&fill(&missing_tmpl, &[("list", &missing.join("; "))]));
            }
            if outcome.skipped > 0 || outcome.errors > 0 {
                status.push_str(&fill(
                    &skipped_tmpl,
                    &[
                        ("s", &outcome.skipped.to_string()),
                        ("e", &outcome.errors.to_string()),
                    ],
                ));
            }
            let mut entries = outcome.entries;
            sort_entries(&mut entries, sort_spec.0, sort_spec.1);
            let _ = tx.send(WorkerMsg::Done(entries, status));
        });
    }

    /// Export every current hit (not just the visible page) in `format`
    /// to a picked file. The write runs on its own thread — streaming
    /// row by row with progress reports — so even million-row exports
    /// never freeze the window. A failed or cancelled run deletes the
    /// partial file instead of leaving a truncated export behind.
    fn on_export_stream(&mut self, kind: &str, ext: &str, format: ExportFormat) {
        if self.export_rx.is_some() {
            self.status = self.tr.t("status.export_running").to_string();
            return;
        }
        let picked = rfd::FileDialog::new()
            .add_filter(kind, &[ext])
            .set_file_name(&format!("supersearch-results.{ext}"))
            .save_file();
        let path = match picked {
            Some(p) => p,
            None => {
                self.status = self.tr.t("status.export_cancelled").to_string();
                return;
            }
        };
        let file = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(e) => {
                self.status = self.tr.f("status.export_failed", &[("e", &e.to_string())]);
                return;
            }
        };
        let entries = self.results.clone();
        let total = entries.len();
        let label = path.display().to_string();
        let (tx, rx) = mpsc::channel();
        self.export_rx = Some(rx);
        self.export_total = total;
        self.export_rows = 0;
        let cancel = Arc::new(AtomicBool::new(false));
        self.export_cancel = Some(Arc::clone(&cancel));
        self.status = self
            .tr
            .f("status.exporting", &[("total", &total.to_string())]);
        // Templates owned into the worker: no borrow crosses the
        // thread boundary.
        let done_tmpl = self.tr.t("status.exported").to_string();
        let cancelled_tmpl = self.tr.t("status.export_cancelled").to_string();
        let failed_tmpl = self.tr.t("status.export_failed").to_string();
        std::thread::spawn(move || {
            let mut out = std::io::BufWriter::new(file);
            let hook = ExportHook {
                on_rows: &|n| {
                    let _ = tx.send(ExportMsg::Progress(n));
                },
                cancel: &cancel,
            };
            match export::write_format(&mut out, &entries, format, Some(&hook)) {
                Ok(()) => {
                    let _ = tx.send(ExportMsg::Done(Ok(fill(
                        &done_tmpl,
                        &[("total", &total.to_string()), ("label", &label)],
                    ))));
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&path);
                    let msg = if e.kind() == std::io::ErrorKind::Interrupted {
                        cancelled_tmpl
                    } else {
                        fill(&failed_tmpl, &[("e", &e.to_string())])
                    };
                    let _ = tx.send(ExportMsg::Done(Err(msg)));
                }
            }
        });
    }

    /// Drain pending export messages; the final message sets the status
    /// and frees the slot for the next export.
    fn poll_export(&mut self) {
        let mut done: Option<Result<String, String>> = None;
        if let Some(rx) = &self.export_rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    ExportMsg::Progress(n) => self.export_rows = n,
                    ExportMsg::Done(result) => {
                        done = Some(result);
                        break;
                    }
                }
            }
        }
        if let Some(result) = done {
            match result {
                Ok(msg) | Err(msg) => self.status = msg,
            }
            self.export_rx = None;
            self.export_cancel = None;
        }
    }

    /// Cancel a running export; the thread aborts at the next row and
    /// removes the partial file.
    fn on_cancel_export(&mut self) {
        if let Some(cancel) = &self.export_cancel {
            cancel.store(true, Ordering::Relaxed);
            self.status = self.tr.t("status.export_canceling").to_string();
        }
    }

    /// Export every current hit (not just the visible page) to a CSV
    /// file chosen with a save dialog, using the picked delimiter.
    fn on_export_csv(&mut self) {
        let delim = self.csv_delim;
        self.on_export_stream("CSV", "csv", ExportFormat::Csv(delim));
    }

    fn on_open_selected(&mut self) {
        let Some(entry) = self.selected.and_then(|i| self.results.get(i)) else {
            self.status = self.tr.t("status.select_row").to_string();
            return;
        };
        match open_path(&entry.path) {
            Ok(()) => {
                self.status = self.tr.f(
                    "status.opened",
                    &[("path", &entry.path.display().to_string())],
                )
            }
            Err(e) => self.status = e,
        }
    }

    fn on_reveal_selected(&mut self) {
        let Some(entry) = self.selected.and_then(|i| self.results.get(i)) else {
            self.status = self.tr.t("status.select_row").to_string();
            return;
        };
        match reveal_path(&entry.path) {
            Ok(()) => {
                self.status = self.tr.f(
                    "status.revealed",
                    &[("path", &entry.path.display().to_string())],
                )
            }
            Err(e) => self.status = e,
        }
    }

    fn on_props_selected(&mut self) {
        let Some(entry) = self.selected.and_then(|i| self.results.get(i)) else {
            self.status = self.tr.t("status.select_row").to_string();
            return;
        };
        match show_file_properties(&entry.path) {
            Ok(()) => {}
            Err(e) => self.status = e,
        }
    }

    /// Copy every bulk-checked path to the clipboard (newline-joined).
    /// A missing clipboard (headless sessions) reports instead of
    /// panicking.
    fn on_copy_checked(&mut self) {
        let paths = self.checked_paths();
        if paths.is_empty() {
            self.status = self.tr.t("status.nothing_checked").to_string();
            return;
        }
        let text = paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let n = paths.len();
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
            Ok(()) => self.status = self.tr.f("status.copied", &[("n", &n.to_string())]),
            Err(e) => self.status = self.tr.f("status.clipboard_fail", &[("e", &e.to_string())]),
        }
    }

    /// Move every bulk-checked row into `dest`, updating result rows
    /// and the path-keyed selection in place. A colliding destination
    /// name is skipped, never overwritten. Returns (moved, skipped).
    fn move_checked_to(&mut self, dest: &Path) -> (usize, usize) {
        let mut moved = 0usize;
        let mut skipped = 0usize;
        let mut renames: Vec<(PathBuf, PathBuf)> = Vec::new();
        for path in self.checked_paths() {
            let Some(name) = path.file_name() else {
                skipped += 1;
                continue;
            };
            let to = dest.join(name);
            if to.exists() {
                skipped += 1;
                continue;
            }
            match std::fs::rename(&path, &to) {
                Ok(()) => renames.push((path, to)),
                Err(_) => skipped += 1,
            }
        }
        for (from, to) in &renames {
            if let Some(slot) = self.results.iter_mut().find(|e| e.path == *from) {
                slot.path = to.clone();
            }
            self.checked.remove(from);
            self.checked.insert(to.clone());
            moved += 1;
        }
        (moved, skipped)
    }

    /// Pick a destination, then confirm: moving is as permanent as
    /// deleting (no undo). Same-volume renames only — a cross-volume
    /// target fails the rename and counts as skipped.
    fn on_move_checked(&mut self) {
        if self.checked.is_empty() {
            self.status = self.tr.t("status.nothing_checked").to_string();
            return;
        }
        let dest = match rfd::FileDialog::new().pick_folder() {
            Some(d) => d,
            None => {
                self.status = self.tr.t("status.move_cancelled").to_string();
                return;
            }
        };
        self.pending_move = Some(dest);
        self.confirm_move = true;
        self.modal_focus_pending = true;
    }

    /// Permanently delete every bulk-checked row (files and directories
    /// alike) and drop them from the results. Called only from the
    /// confirmation dialog — there is no recycle-bin round trip.
    fn on_delete_checked(&mut self) {
        self.confirm_delete = false;
        let targets = self.checked_paths();
        if targets.is_empty() {
            self.status = self.tr.t("status.nothing_checked").to_string();
            return;
        }
        let mut gone: Vec<PathBuf> = Vec::new();
        let mut errors = 0usize;
        let mut first_error = String::new();
        for p in &targets {
            let outcome = if p.is_dir() {
                std::fs::remove_dir_all(p)
            } else {
                std::fs::remove_file(p)
            };
            match outcome {
                Ok(()) => gone.push(p.clone()),
                Err(e) => {
                    errors += 1;
                    if first_error.is_empty() {
                        first_error = format!("{}: {e}", p.display());
                    }
                }
            }
        }
        let removed = gone.len();
        self.results.retain(|e| !gone.contains(&e.path));
        for p in &gone {
            self.checked.remove(p);
        }
        self.selected = None;
        self.status = if errors == 0 {
            self.tr.f("status.deleted", &[("n", &removed.to_string())])
        } else {
            self.tr.f(
                "status.deleted_errors",
                &[
                    ("n", &removed.to_string()),
                    ("e", &errors.to_string()),
                    ("first", &first_error),
                ],
            )
        };
    }
}

fn opt_text(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// True when `path` is hidden right now (attribute bit on Windows,
/// dotfile elsewhere). Used to honor the per-tab Show-hidden toggle on
/// indexed searches, whose rows were stored with metadata but without a
/// hidden flag. Unstatable paths count as visible: a stale index row
/// still answers the query, and pruning it is the indexer's job.
fn entry_hidden(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|md| entry_is_hidden(path, &md))
        .unwrap_or(false)
}

/// Parse an optional GUI date field with the shared
/// [`supersearch_query::parse_mtime`] implementation (Unix timestamp or
/// `YYYY-MM-DD`), so the GUI and CLI accept exactly the same inputs.
fn opt_mtime(s: &str, flag: &str) -> Result<Option<i64>, String> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    supersearch_query::parse_mtime(t)
        .map(Some)
        .map_err(|e| format!("{flag}: {e}"))
}

/// Split a comma-separated exclusion field into patterns.
fn parse_exclusions(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// Byte ranges of every non-overlapping `needle` occurrence in
/// `text`. Empty needle matches nothing. Case-insensitive folding
/// applies to ASCII only (lowercasing other scripts can shift byte
/// offsets, so those fall back to a sensitive search).
fn highlight_ranges(text: &str, needle: &str, case_insensitive: bool) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let fold = case_insensitive && text.is_ascii() && needle.is_ascii();
    let (hay, ndl) = if fold {
        (text.to_lowercase(), needle.to_lowercase())
    } else {
        (text.to_string(), needle.to_string())
    };
    let mut ranges = Vec::new();
    let mut from = 0usize;
    while let Some(pos) = hay[from..].find(&ndl) {
        let start = from + pos;
        ranges.push((start, start + ndl.len()));
        from = start + ndl.len();
    }
    ranges
}

/// Build an egui text job with `ranges` highlighted over `text`.
/// Ranges should be byte boundaries into `text` (as produced by
/// [`highlight_ranges`]); anything unusable is skipped, never panics.
/// Every span uses `font` so jobs blend into monospace and
/// proportional contexts alike.
fn highlight_job(
    text: &str,
    ranges: &[(usize, usize)],
    normal: egui::Color32,
    font: egui::FontId,
) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let mut job = LayoutJob::default();
    let mut push = |piece: &str, highlighted: bool| {
        if piece.is_empty() {
            return;
        }
        let mut format = TextFormat {
            font_id: font.clone(),
            color: if highlighted { HIGHLIGHT_FG } else { normal },
            ..TextFormat::default()
        };
        if highlighted {
            format.background = HIGHLIGHT_BG;
        }
        job.append(piece, 0.0, format);
    };
    let mut cursor = 0usize;
    for &(start, end) in ranges {
        let (start, end) = (start.min(text.len()), end.min(text.len()));
        if end <= start || start >= text.len() {
            continue;
        }
        if start > cursor {
            if let Some(gap) = text.get(cursor..start) {
                push(gap, false);
            }
            cursor = start;
        }
        if end > cursor {
            if let Some(piece) = text.get(cursor..end) {
                push(piece, true);
            }
            cursor = end;
        }
    }
    if let Some(tail) = text.get(cursor.min(text.len())..) {
        push(tail, false);
    }
    job
}

/// Human file size: `0 B`, `1.4 KB`, `23.0 MB`, `2.1 GB`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", bytes)
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Gregorian `(year, month, day)` for whole `days` since the Unix epoch
/// (Howard Hinnant's civil-from-days; handles pre-1970 too).
fn ymd_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y } as i32, m, d)
}

/// `2024-05-01 12:30`, plus a relative age for the last ~10 years.
/// Future stamps clamp to `0d ago` instead of rendering negative ages.
/// Compact `YYYY-MM-DD` stamp for the results table (the full
/// [`mtime_display`] shows on hover and in the preview).
fn mtime_short(ts: i64) -> String {
    let (y, m, d) = ymd_from_days(ts.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn mtime_display(ts: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(ts);
    let age_days = ((now - ts) / 86_400).max(0);
    let hour = (ts.rem_euclid(86_400) / 3_600) as u32;
    let min = (ts.rem_euclid(3_600) / 60) as u32;
    let (y, m, d) = ymd_from_days(ts.div_euclid(86_400));
    let date = format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02}");
    if age_days < 3650 {
        format!("{date} ({age_days}d ago)")
    } else {
        date
    }
}

/// First occurrence of `needle` in `text` as `(byte_pos, byte_len)`,
/// honoring case-insensitivity for ASCII (same fold rule as
/// [`highlight_ranges`]).
fn find_needle(text: &str, needle: &str, case_insensitive: bool) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    if case_insensitive && text.is_ascii() && needle.is_ascii() {
        let hay = text.to_lowercase();
        let ndl = needle.to_lowercase();
        hay.find(&ndl).map(|pos| (pos, ndl.len()))
    } else {
        text.find(needle).map(|pos| (pos, needle.len()))
    }
}

/// Read a short preview of `entry`'s contents for the preview pane.
///
/// Shows the region around the first `needle` match (when set and found)
/// or the head of the file otherwise. Bytes decode with the query
/// crate's encoding sniffer, so UTF-16 files preview as text; only the
/// first 64 KiB are ever read.
fn content_snippet(entry: &FileEntry, needle: Option<&str>, case_insensitive: bool) -> String {
    if entry.is_dir {
        return "(directory — no file contents to preview.)".to_string();
    }
    const MAX_READ: u64 = 64 * 1024;
    const CONTEXT: usize = 600;
    const HEAD: usize = 2000;
    let file = match std::fs::File::open(&entry.path) {
        Ok(f) => f,
        Err(e) => return format!("(cannot open file: {e})"),
    };
    use std::io::Read as _;
    let mut buf = Vec::new();
    let mut limited = file.take(MAX_READ + 1);
    if limited.read_to_end(&mut buf).is_err() {
        return "(cannot read file.)".to_string();
    }
    let truncated = buf.len() as u64 > MAX_READ;
    if truncated {
        buf.truncate(MAX_READ as usize);
    }
    let text = decode_text_bytes(&buf);
    let body = if let Some(n) = needle {
        let n = n.trim();
        if n.is_empty() {
            take_head(&text, HEAD)
        } else if let Some((pos, len)) = find_needle(&text, n, case_insensitive) {
            let start = pos.saturating_sub(CONTEXT);
            let end = (pos + len + CONTEXT).min(text.len());
            // Snap to char boundaries so slicing never panics.
            let mut s = start;
            while s < pos && !text.is_char_boundary(s) {
                s += 1;
            }
            let mut e = end;
            while e > pos + len && !text.is_char_boundary(e) {
                e -= 1;
            }
            let mut out = String::new();
            if s > 0 {
                out.push_str("…");
            }
            out.push_str(&text[s..e]);
            if truncated || e < text.len() {
                out.push('…');
            }
            out
        } else {
            format!(
                "(no {n:?} match in first 64 KiB.)\n{}",
                take_head(&text, HEAD)
            )
        }
    } else {
        take_head(&text, HEAD)
    };
    if truncated && !body.ends_with('…') {
        format!("{body}…")
    } else {
        body
    }
}

fn take_head(text: &str, n: usize) -> String {
    let mut end = text.len().min(n);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Open a path with the OS default handler (no extra dependencies).
fn open_path(path: &Path) -> Result<(), String> {
    #[cfg(not(target_os = "windows"))]
    let target = path.to_string_lossy().into_owned();
    #[cfg(target_os = "windows")]
    return supersearch_core::shell::open_path(path);
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(&target).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(&target).spawn();
    #[cfg(not(target_os = "windows"))]
    r.map(|_| ())
        .map_err(|e| format!("Open failed for {}: {e}", path.display()))
}

#[cfg(windows)]
fn show_file_properties(path: &Path) -> Result<(), String> {
    supersearch_core::shell::show_file_properties(path)
}

/// File properties outside Windows: no single native dialog exists
/// (the preview pane already shows path, size, and modified time).
#[cfg(not(windows))]
fn show_file_properties(path: &Path) -> Result<(), String> {
    Err(format!(
        "Properties are only available on Windows ({}).",
        path.display()
    ))
}

/// Reveal a path in the OS file manager (select it when supported).
fn reveal_path(path: &Path) -> Result<(), String> {
    let target = path.to_string_lossy().into_owned();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("explorer")
        .arg(format!("/select,{target}"))
        .spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open")
        .args(["-R", &target])
        .spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let r = {
        let parent = path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(target);
        std::process::Command::new("xdg-open").arg(&parent).spawn()
    };
    r.map(|_| ())
        .map_err(|e| format!("Reveal failed for {}: {e}", path.display()))
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Best-effort persist on exit so roots, db path, window state,
        // saved searches, recents, and exclusions survive restarts.
        let _ = self.config.save_to(&self.config_path);
        // Print views must not accumulate in the temp dir.
        if let Some(path) = self.last_print.take() {
            let _ = std::fs::remove_file(path);
        }
        // Signal the indexer to exit; no join — a mid-fill tick must
        // not delay window close, and process exit reaps the thread.
        if let Some(handle) = self.daemon.take() {
            handle.stop();
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Interface scale (Display settings, persisted): uniform text
        // and widget sizing, sanitized so a hand-edited config cannot
        // break layout.
        ctx.set_pixels_per_point(supersearch_config::sanitize_ui_scale(self.config.ui_scale));
        ctx.set_visuals(if self.dark_mode {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        self.poll_worker();
        self.poll_export();
        self.poll_daemon_status();
        self.maybe_live_search();
        // Keyboard: Enter searches (outside dialogs and mid-scan);
        // arrows move the selection and Space toggles its checkbox
        // whenever no text field holds focus (so typing keeps working).
        let modal_open = self.confirm_delete || self.confirm_move;
        // Enter always searches, even mid-typing: the core type+Enter
        // flow must work from any filter field.
        if !modal_open && ctx.input(|i| i.key_pressed(egui::Key::Enter)) && !self.scanning {
            self.on_search();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if modal_open {
                self.confirm_delete = false;
                self.confirm_move = false;
                self.pending_move = None;
            } else if let Some(id) = ctx.memory(|m| m.focused()) {
                // Esc drops text-field focus so arrows/Space reach the
                // table again (no focus trap).
                ctx.memory_mut(|m| m.surrender_focus(id));
            }
        }
        // Arrows/Space need the table focused, so they stay gated on
        // no text field holding focus (typing keeps working).
        let typing = ctx.memory(|m| m.focused().is_some());
        if !modal_open && !typing {
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                self.step_selection(-1);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                self.step_selection(1);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
                self.toggle_selected_check();
            }
        }
        // Track window geometry every frame so `on_exit` writes fresh state.
        if let Some(size) = ctx.input(|i| i.viewport().inner_rect.map(|r| (r.width(), r.height())))
        {
            self.config.window.width = size.0;
            self.config.window.height = size.1;
            self.config.window.maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        }
        if self.scanning {
            ctx.request_repaint();
        }
        if self.daemon.is_some() {
            // Wake ~1 Hz for the indexer "Ns ago" readout; the status
            // recompute itself is separately gated in
            // `poll_daemon_status`.
            ctx.request_repaint_after(DAEMON_STATUS_EVERY);
        }

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 4.0);
            // Wrapped: narrow windows fold the toggles onto a second
            // line instead of pushing Search off-screen.
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(icons::MAGNIFYING_GLASS)
                        .color(ACCENT)
                        .size(18.0),
                );
                ui.heading("SuperSearch");
                // Static glyphs with selected-state styling: the icon
                // names the feature, the highlight shows the state.
                if ui
                    .selectable_label(self.dark_mode, icons::MOON)
                    .on_hover_text(self.tr.t("toolbar.theme"))
                    .clicked()
                {
                    self.dark_mode = !self.dark_mode;
                }
                if ui
                    .selectable_label(self.show_filters, icons::SLIDERS_HORIZONTAL)
                    .on_hover_text(self.tr.t("toolbar.filters"))
                    .clicked()
                {
                    self.show_filters = !self.show_filters;
                }
                if ui
                    .selectable_label(self.show_preview, icons::EYE)
                    .on_hover_text(self.tr.t("toolbar.preview"))
                    .clicked()
                {
                    self.show_preview = !self.show_preview;
                }
                if ui
                    .small_button(format!("{} {}", icons::QUESTION, self.tr.t("toolbar.help")))
                    .on_hover_text(self.tr.t("toolbar.help_hover"))
                    .clicked()
                {
                    self.show_help = true;
                }
                if ui
                    .small_button(format!(
                        "{} {}",
                        icons::PLUGS_CONNECTED,
                        self.tr.t("toolbar.integration")
                    ))
                    .on_hover_text(self.tr.t("toolbar.integration_hover"))
                    .clicked()
                {
                    self.show_integration = true;
                }
                if ui
                    .small_button(format!(
                        "{} {}",
                        icons::TEXT_AA,
                        self.tr.t("toolbar.display")
                    ))
                    .on_hover_text(self.tr.t("toolbar.display_hover"))
                    .clicked()
                {
                    self.show_display = true;
                }
                if ui
                    .small_button(format!("{} {}", icons::TRANSLATE, self.lang.label()))
                    .on_hover_text(self.tr.t("toolbar.lang_hover"))
                    .clicked()
                {
                    self.lang_filter.clear();
                    self.lang_pick_open = true;
                }
            });
            ui.horizontal(|ui| {
                let indexed_label = self.tr.t("toolbar.indexed");
                ui.checkbox(&mut self.use_indexed, indexed_label.as_str())
                    .on_hover_text(self.tr.t("toolbar.indexed_hover"));
                ui.label(self.tr.t("toolbar.db"));
                ui.add(egui::TextEdit::singleline(&mut self.db_path).desired_width(240.0))
                    .on_hover_text(self.tr.t("toolbar.db_hover"));
                if ui
                    .button(format!(
                        "{} {}",
                        icons::FOLDER_OPEN,
                        self.tr.t("toolbar.finddb")
                    ))
                    .on_hover_text(self.tr.t("toolbar.finddb_hover"))
                    .clicked()
                {
                    if let Some(file) = rfd::FileDialog::new().pick_file() {
                        self.db_path = file.display().to_string();
                    }
                }
                let indexer_icon = if self.daemon.is_some() {
                    icons::STOP
                } else {
                    icons::PLAY
                };
                let indexer_label = if self.daemon.is_some() {
                    self.tr.t("toolbar.indexer_stop")
                } else {
                    self.tr.t("toolbar.indexer_start")
                };
                if ui
                    .button(format!("{indexer_icon} {indexer_label}"))
                    .on_hover_text(self.tr.t("toolbar.indexer_hover"))
                    .clicked()
                {
                    self.on_daemon_toggle();
                }
            });
        });
        // Search tabs: each owns its folders, filters, results, and
        // worker. `*` means the filters changed since the last run;
        // `…` means a scan is still running (even in the background).
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let db = self.db_path.clone();
                let mut switch_to: Option<usize> = None;
                let mut close: Option<usize> = None;
                for i in 0..self.tabs.len() {
                    let tab = &self.tabs[i];
                    let dirty = tab.searched && tab.last_search_key != tab.filter_key(&db);
                    let mut title = self.tab_title(i);
                    if tab.scanning {
                        title.push('…');
                    } else if dirty {
                        title.push_str(" *");
                    }
                    let hover = if tab.scanning {
                        self.tr.t("tab.switch_scanning")
                    } else {
                        self.tr.t("tab.switch")
                    };
                    if ui
                        .selectable_label(i == self.active, title)
                        .on_hover_text(hover)
                        .clicked()
                    {
                        switch_to = Some(i);
                    }
                    if self.tabs.len() > 1
                        && ui
                            .small_button("x")
                            .on_hover_text(self.tr.t("tab.close_hover"))
                            .clicked()
                    {
                        close = Some(i);
                    }
                }
                if ui
                    .small_button(format!("{} {}", icons::PLUS, self.tr.t("tab.new")))
                    .on_hover_text(self.tr.t("tab.new_hover"))
                    .clicked()
                {
                    self.new_tab();
                }
                if let Some(i) = switch_to {
                    self.active = i;
                }
                if let Some(i) = close {
                    self.close_tab(i);
                }
            });
        });
        if self.show_filters {
            egui::SidePanel::left("filters")
                .resizable(true)
                .default_width(300.0)
                .show(ctx, |ui| {
                    ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 4.0);
                    // Scrollable: short windows must still reach
                    // Saved/Recent at the bottom.
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.horizontal(|ui| {
                            section_heading(
                                ui,
                                icons::SLIDERS_HORIZONTAL,
                                self.tr.t("filters.title"),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .button(format!(
                                            "{} {}",
                                            icons::BROOM,
                                            self.tr.t("filters.clear")
                                        ))
                                        .on_hover_text(self.tr.t("filters.clear_hover"))
                                        .clicked()
                                    {
                                        self.clear_filters();
                                    }
                                },
                            );
                        });
                        section_heading(ui, icons::FUNNEL, self.tr.t("filters.match"));
                        let name_hint = self.tr.t("filters.name_hint");
                        let regex_hint = self.tr.t("filters.regex_hint");
                        let fuzzy_hint = self.tr.t("filters.fuzzy_hint");
                        let content_hint = self.tr.t("filters.content_hint");
                        let glob_hint = self.tr.t("filters.glob_hint");
                        let or_hint = self.tr.t("filters.or_hint");
                        let not_hint = self.tr.t("filters.not_hint");
                        egui::Grid::new("filters")
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                ui.label(self.tr.t("filters.name"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.name)
                                        .hint_text(name_hint.as_str()),
                                )
                                .on_hover_text(self.tr.t("filters.name_hover"));
                                ui.end_row();
                                ui.label(self.tr.t("filters.regex"));
                                // Invalid regexes tint red inline instead of
                                // surfacing only as status-bar text.
                                let regex_ok =
                                    self.regex.trim().is_empty() || regex_is_valid(&self.regex);
                                let mut regex_edit = egui::TextEdit::singleline(&mut self.regex)
                                    .hint_text(regex_hint.as_str());
                                if !regex_ok {
                                    regex_edit = regex_edit
                                        .text_color(egui::Color32::from_rgb(255, 110, 110));
                                }
                                ui.add(regex_edit)
                                    .on_hover_text(self.tr.t("filters.regex_hover"));
                                ui.end_row();
                                ui.label(self.tr.t("filters.fuzzy"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.fuzzy)
                                        .hint_text(fuzzy_hint.as_str()),
                                )
                                .on_hover_text(self.tr.t("filters.fuzzy_hover"));
                                ui.end_row();
                                ui.label(self.tr.t("filters.content"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.content)
                                        .hint_text(content_hint.as_str()),
                                )
                                .on_hover_text(self.tr.t("filters.content_hover"));
                                ui.end_row();
                                ui.label(self.tr.t("filters.glob"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.glob)
                                        .hint_text(glob_hint.as_str()),
                                )
                                .on_hover_text(self.tr.t("filters.glob_hover"));
                                ui.end_row();
                                ui.label(self.tr.t("filters.or"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.or_terms)
                                        .hint_text(or_hint.as_str()),
                                )
                                .on_hover_text(self.tr.t("filters.or_hover"));
                                ui.end_row();
                                ui.label(self.tr.t("filters.not"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.not_terms)
                                        .hint_text(not_hint.as_str()),
                                )
                                .on_hover_text(self.tr.t("filters.not_hover"));
                                ui.end_row();
                            });
                        section_heading(ui, icons::RULER, self.tr.t("filters.sizedate"));
                        let size_hint = self.tr.t("filters.size_hint");
                        let date_hint = self.tr.t("filters.date_hint");
                        let exclude_hint = self.tr.t("filters.exclude_hint");
                        egui::Grid::new("sizedate")
                            .num_columns(2)
                            .spacing([8.0, 4.0])
                            .show(ui, |ui| {
                                ui.label(self.tr.t("filters.min"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.min_size)
                                        .hint_text(size_hint.as_str()),
                                )
                                .on_hover_text(self.tr.t("filters.min_hover"));
                                ui.end_row();
                                ui.label(self.tr.t("filters.max"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.max_size)
                                        .hint_text(size_hint.as_str()),
                                )
                                .on_hover_text(self.tr.t("filters.max_hover"));
                                ui.end_row();
                                ui.label(self.tr.t("filters.after"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.modified_after)
                                        .hint_text(date_hint.as_str()),
                                )
                                .on_hover_text(self.tr.t("filters.after_hover"));
                                ui.end_row();
                                ui.label(self.tr.t("filters.before"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.modified_before)
                                        .hint_text(date_hint.as_str()),
                                )
                                .on_hover_text(self.tr.t("filters.before_hover"));
                                ui.end_row();
                                ui.label(self.tr.t("filters.exclude"));
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.exclusions)
                                        .hint_text(exclude_hint.as_str()),
                                )
                                .on_hover_text(self.tr.t("filters.exclude_hover"));
                                ui.end_row();
                            });
                        section_heading(ui, icons::FILE, self.tr.t("filters.file"));
                        ui.label(self.tr.t("filters.ext"));
                        let ext_hint = self.tr.t("filters.ext_hint");
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut self.extensions)
                                    .hint_text(ext_hint.as_str()),
                            )
                            .on_hover_text(self.tr.t("filters.ext_hover"))
                            .changed()
                        {
                            // Hand edits detach the chip highlight: the
                            // field no longer equals any group.
                            self.group_pick = None;
                        }
                        ui.horizontal_wrapped(|ui| {
                            ui.label(self.tr.t("filters.groups"));
                            // Chips REPLACE the extension field (never
                            // append); clicking the active chip clears it
                            // again. Custom groups live in Manage.
                            let groups = self.config.all_file_groups();
                            let mut pick: Option<Option<FileGroup>> = None;
                            for g in &groups {
                                let active = self.group_pick.as_deref() == Some(g.name.as_str());
                                if ui
                                    .selectable_label(active, &g.name)
                                    .on_hover_text(self.tr.f(
                                        "filters.groups_set",
                                        &[("list", &g.extensions.join(", "))],
                                    ))
                                    .clicked()
                                {
                                    pick = Some(if active { None } else { Some(g.clone()) });
                                }
                            }
                            if ui
                                .small_button(self.tr.t("filters.manage"))
                                .on_hover_text(self.tr.t("filters.manage_hover"))
                                .clicked()
                            {
                                self.manage_groups = true;
                            }
                            if let Some(p) = pick {
                                match p {
                                    Some(g) => {
                                        self.extensions = g.extensions.join(", ");
                                        self.group_pick = Some(g.name);
                                    }
                                    None => {
                                        self.extensions.clear();
                                        self.group_pick = None;
                                    }
                                }
                            }
                        });
                        ui.horizontal_wrapped(|ui| {
                            let ignorecase_label = self.tr.t("filters.ignorecase");
                            let showhidden_label = self.tr.t("filters.showhidden");
                            ui.checkbox(&mut self.case_insensitive, ignorecase_label.as_str())
                                .on_hover_text(self.tr.t("filters.ignorecase_hover"));
                            ui.checkbox(&mut self.show_hidden, showhidden_label.as_str())
                                .on_hover_text(self.tr.t("filters.showhidden_hover"));
                        });
                        section_heading(ui, icons::GEAR, self.tr.t("filters.presets"));
                        ui.horizontal_wrapped(|ui| {
                            if self.config.presets.is_empty() {
                                ui.weak(self.tr.t("filters.presets_none"));
                            } else {
                                for preset in self.config.presets.clone() {
                                    if ui
                                        .button(&preset.name)
                                        .on_hover_text(self.tr.t("filters.presets_hover"))
                                        .clicked()
                                    {
                                        self.exclusions = preset.exclusions.join(", ");
                                        self.extensions = preset.extensions.join(", ");
                                        // A preset is its own slice, not a group pick.
                                        self.group_pick = None;
                                    }
                                }
                            }
                        });
                        section_heading(ui, icons::STAR, self.tr.t("filters.saved"));
                        ui.horizontal(|ui| {
                            let names: Vec<String> = self
                                .config
                                .saved_searches
                                .iter()
                                .map(|s| s.name.clone())
                                .collect();
                            if names.is_empty() {
                                ui.weak(self.tr.t("filters.saved_none"));
                            }
                            egui::ComboBox::from_id_salt("saved_pick")
                                .selected_text(self.saved_pick.clone())
                                .show_ui(ui, |ui| {
                                    for name in &names {
                                        ui.selectable_value(
                                            &mut self.saved_pick,
                                            name.clone(),
                                            name,
                                        );
                                    }
                                })
                                .response
                                .on_hover_text(self.tr.t("filters.saved_combo_hover"));
                            if ui
                                .button(format!(
                                    "{} {}",
                                    icons::FOLDER_OPEN,
                                    self.tr.t("filters.saved_load")
                                ))
                                .on_hover_text(self.tr.t("filters.saved_load_hover"))
                                .clicked()
                                && !self.saved_pick.is_empty()
                            {
                                if let Some(saved) = self
                                    .config
                                    .saved_searches
                                    .iter()
                                    .find(|s| s.name == self.saved_pick)
                                    .cloned()
                                {
                                    self.apply_saved_search(&saved);
                                    self.status = self.tr.f(
                                        "status.saved_loaded",
                                        &[("name", &format!("{:?}", saved.name))],
                                    );
                                }
                            }
                            if ui
                                .button(format!(
                                    "{} {}",
                                    icons::TRASH,
                                    self.tr.t("filters.saved_delete")
                                ))
                                .on_hover_text(self.tr.t("filters.saved_delete_hover"))
                                .clicked()
                                && !self.saved_pick.is_empty()
                            {
                                let name = self.saved_pick.clone();
                                if self.config.remove_saved_search(&name) {
                                    self.saved_pick.clear();
                                    self.persist();
                                    self.status = self.tr.f(
                                        "status.saved_deleted",
                                        &[("name", &format!("{name:?}"))],
                                    );
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label(self.tr.t("filters.saved_name"));
                            let saved_name_hint = self.tr.t("filters.saved_name_hint");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.save_name)
                                    .hint_text(saved_name_hint.as_str()),
                            )
                            .on_hover_text(self.tr.t("filters.saved_name_hover"));
                            if ui
                                .button(format!(
                                    "{} {}",
                                    icons::FLOPPY_DISK,
                                    self.tr.t("filters.saved_save")
                                ))
                                .on_hover_text(self.tr.t("filters.saved_save_hover"))
                                .clicked()
                            {
                                let name = self.save_name.trim().to_string();
                                if name.is_empty() {
                                    self.status = self.tr.t("status.name_first").to_string();
                                } else {
                                    match self.current_saved_search(&name) {
                                        Ok(saved) => {
                                            self.config.add_saved_search(saved);
                                            self.saved_pick = name.clone();
                                            self.persist();
                                            self.status = self.tr.f(
                                                "status.saved_saved",
                                                &[("name", &format!("{name:?}"))],
                                            );
                                        }
                                        Err(e) => {
                                            self.status =
                                                self.tr.f("status.bad_filter", &[("e", &e)]);
                                        }
                                    }
                                }
                            }
                        });
                        if !self.config.recents.is_empty() {
                            section_heading(
                                ui,
                                icons::CLOCK_COUNTER_CLOCKWISE,
                                self.tr.t("filters.recent"),
                            );
                            ui.horizontal_wrapped(|ui| {
                                let recents = self.config.recents.clone();
                                for recent in recents.iter().take(5) {
                                    // Snapshots restore every filter; legacy bare
                                    // names restore just the name filter.
                                    let tip = match recent {
                                        RecentEntry::Full(saved) => {
                                            let where_ = if saved.root.trim().is_empty() {
                                                self.tr.t("recent.current_folders").to_string()
                                            } else {
                                                saved.root.clone()
                                            };
                                            self.tr.f(
                                                "recent.full",
                                                &[
                                                    ("name", &saved.name),
                                                    ("filters", &recent.filters_summary()),
                                                    ("where", &where_),
                                                ],
                                            )
                                        }
                                        RecentEntry::Legacy(name) => {
                                            self.tr.f("recent.legacy", &[("name", name)])
                                        }
                                    };
                                    if ui.button(recent.label()).on_hover_text(tip).clicked() {
                                        match recent {
                                            RecentEntry::Full(saved) => {
                                                self.apply_saved_search(saved)
                                            }
                                            RecentEntry::Legacy(name) => self.name = name.clone(),
                                        }
                                    }
                                }
                            });
                        }
                    }); // end filter scroll area
                }); // end SidePanel::left("filters")
        } // end `if self.show_filters`

        // Custom file-group manager: customs add/replace by name and
        // can be deleted (revealing any shadowed built-in); built-ins
        // are read-only. Config writes persist after the modal closes
        // its borrow.
        if self.manage_groups {
            let mut close = false;
            let mut changed = false;
            let mut deletes: Vec<String> = Vec::new();
            egui::Modal::new(egui::Id::new("manage_groups")).show(ctx, |ui| {
                ui.set_min_width(380.0);
                ui.heading(format!(
                    "{} {}",
                    icons::FOLDERS,
                    self.tr.t("modal.groups_title")
                ));
                ui.weak(self.tr.t("modal.groups_sub"));
                let all = self.config.all_file_groups();
                for g in &all {
                    let custom = self
                        .config
                        .file_groups
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(&g.name));
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&g.name).strong());
                        if custom {
                            if ui
                                .small_button("x")
                                .on_hover_text(self.tr.t("modal.groups_delete_hover"))
                                .clicked()
                            {
                                deletes.push(g.name.clone());
                            }
                        } else {
                            ui.weak(self.tr.t("modal.groups_builtin"));
                        }
                    });
                    ui.weak(g.extensions.join(", "));
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(self.tr.t("modal.groups_name"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_group_name)
                            .hint_text(self.tr.t("modal.groups_name_hint")),
                    )
                    .on_hover_text(self.tr.t("modal.groups_name_hover"));
                    ui.label(self.tr.t("modal.groups_ext"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.new_group_exts)
                            .hint_text(self.tr.t("modal.groups_ext_hint")),
                    )
                    .on_hover_text(self.tr.t("modal.groups_ext_hover"));
                    if ui
                        .button(format!("{} {}", icons::PLUS, self.tr.t("modal.groups_add")))
                        .on_hover_text(self.tr.t("modal.groups_add_hover"))
                        .clicked()
                    {
                        let group = FileGroup {
                            name: self.new_group_name.trim().to_string(),
                            extensions: self
                                .new_group_exts
                                .split(',')
                                .map(|e| e.trim().to_string())
                                .filter(|e| !e.is_empty())
                                .collect(),
                        };
                        match self.config.upsert_file_group(group) {
                            Ok(()) => {
                                let saved = self.new_group_name.trim().to_string();
                                self.new_group_name.clear();
                                self.new_group_exts.clear();
                                changed = true;
                                self.tab_mut().status = self
                                    .tr
                                    .f("status.group_saved", &[("name", &format!("{saved:?}"))]);
                            }
                            Err(e) => {
                                self.tab_mut().status =
                                    self.tr.f("status.group_bad", &[("e", &e.to_string())]);
                            }
                        }
                    }
                });
                if ui.button(self.tr.t("modal.groups_close")).clicked() {
                    close = true;
                }
            });
            for name in deletes {
                if self.config.remove_file_group(&name) {
                    changed = true;
                }
            }
            if changed {
                self.persist();
            }
            if close {
                self.manage_groups = false;
            }
        }

        // Language picker: a searchable modal (the list is ~90
        // entries, far past what a ComboBox handles). The filter
        // matches the native name, the English name, or the ISO code.
        if self.lang_pick_open {
            let mut close = false;
            let mut pick: Option<Lang> = None;
            let title = self
                .tr
                .f("lang.title", &[("n", &Lang::all().len().to_string())]);
            egui::Modal::new(egui::Id::new("lang_pick")).show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.heading(title);
                let search_hint = self.tr.t("lang.search_hint");
                ui.add(
                    egui::TextEdit::singleline(&mut self.lang_filter)
                        .hint_text(search_hint.as_str()),
                )
                .on_hover_text(self.tr.t("lang.search_hover"));
                let q = self.lang_filter.trim().to_lowercase();
                let mut shown = 0;
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for l in Lang::all() {
                            let hit = q.is_empty()
                                || l.label().to_lowercase().contains(&q)
                                || l.english().to_lowercase().contains(&q)
                                || l.code().to_lowercase().contains(&q);
                            if !hit {
                                continue;
                            }
                            shown += 1;
                            if ui
                                .selectable_label(
                                    self.lang == l,
                                    format!("{} · {}", l.label(), l.code()),
                                )
                                .clicked()
                            {
                                pick = Some(l);
                                close = true;
                            }
                        }
                    });
                if shown == 0 {
                    ui.weak(self.tr.t("lang.empty"));
                }
            });
            if let Some(l) = pick {
                self.set_language(l);
            }
            if close {
                self.lang_pick_open = false;
            }
        }

        // System integration: right-click folder entries (Explorer on
        // Windows, Nautilus/Dolphin/Nemo actions on Linux). HKCU on
        // Windows needs no admin; Linux writes user-level action files
        // and needs supersearch-gui on PATH.
        if self.show_integration {
            let mut close = false;
            let mut apply: Option<bool> = None;
            let title = self.tr.t("integration.title");
            egui::Modal::new(egui::Id::new("integration")).show(ctx, |ui| {
                ui.set_min_width(380.0);
                ui.heading(title);
                let mut on = Self::context_menu_installed();
                let resp = ui.checkbox(&mut on, self.tr.t("integration.folder"));
                let toggled = resp.changed();
                resp.on_hover_text(self.tr.t("integration.folder_hint"));
                if toggled {
                    apply = Some(on);
                }
                if ui.button(self.tr.t("run.cancel")).clicked() {
                    close = true;
                }
            });
            if let Some(on) = apply {
                match Self::set_context_menu(on) {
                    Ok(()) => {
                        self.status = self.tr.t(if on {
                            "integration.enabled"
                        } else {
                            "integration.disabled"
                        });
                    }
                    Err(e) => {
                        self.status = self.tr.f("integration.fail", &[("e", &e)]);
                    }
                }
            }
            if close {
                self.show_integration = false;
            }
        }

        // Display settings: interface scale slider (50–200%), live
        // and persisted. Uniform egui scaling, so every label, row,
        // and modal grows together.
        if self.show_display {
            let mut close = false;
            let title = self.tr.t("display.title");
            egui::Modal::new(egui::Id::new("display")).show(ctx, |ui| {
                ui.set_min_width(320.0);
                ui.heading(title);
                ui.horizontal(|ui| {
                    ui.label(self.tr.t("display.scale"));
                    let resp = ui.add(egui::Slider::new(&mut self.config.ui_scale, 0.5..=2.0));
                    if resp.changed() {
                        self.config.ui_scale =
                            supersearch_config::sanitize_ui_scale(self.config.ui_scale);
                        self.persist();
                    }
                    ui.label(format!("{}%", (self.config.ui_scale * 100.0).round()));
                });
                ui.horizontal(|ui| {
                    if ui.button(self.tr.t("display.reset")).clicked() {
                        self.config.ui_scale = 1.0;
                        self.persist();
                    }
                    if ui.button(self.tr.t("run.cancel")).clicked() {
                        close = true;
                    }
                });
            });
            if close {
                self.show_display = false;
            }
        }

        // In-app help: the same field guide as the tooltips, plus the
        // workflow and keys, in one place.
        if self.show_help {
            let mut close = false;
            egui::Modal::new(egui::Id::new("help")).show(ctx, |ui| {
                ui.set_min_width(460.0);
                ui.heading(format!(
                    "{} {}",
                    icons::QUESTION,
                    self.tr.t("modal.help_title")
                ));
                egui::ScrollArea::vertical()
                    .max_height(420.0)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(self.tr.t("modal.help_s1t")).strong());
                        ui.weak(self.tr.t("modal.help_s1b"));
                        ui.label(egui::RichText::new(self.tr.t("modal.help_s2t")).strong());
                        ui.weak(self.tr.t("modal.help_s2b"));
                        ui.label(egui::RichText::new(self.tr.t("modal.help_s3t")).strong());
                        ui.weak(self.tr.t("modal.help_s3b"));
                        ui.label(egui::RichText::new(self.tr.t("modal.help_s4t")).strong());
                        ui.weak(self.tr.t("modal.help_s4b"));
                        ui.label(egui::RichText::new(self.tr.t("modal.help_keys")).strong());
                        ui.weak(self.tr.t("modal.help_keys_body"));
                    });
                if ui.button(self.tr.t("modal.groups_close")).clicked() {
                    close = true;
                }
            });
            if close {
                self.show_help = false;
            }
        }

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 4.0);
            ui.horizontal(|ui| {
                if self.scanning {
                    if ui
                        .button(format!("{} {}", icons::X, self.tr.t("run.cancel")))
                        .on_hover_text(self.tr.t("run.cancel_hover"))
                        .clicked()
                    {
                        self.on_cancel();
                    }
                    ui.add(egui::Spinner::new().size(16.0));
                    ui.label(
                        self.tr
                            .f("status.scanned", &[("n", &self.scanned.to_string())]),
                    )
                    .on_hover_text(self.tr.t("status.scanned_hover"));
                } else {
                    // Hover repeats the full text: long hit counts get
                    // ellipsized in narrow windows.
                    ui.label(&self.status).on_hover_text(self.status.clone());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let daemon_line = if self.daemon_status.is_empty() {
                        self.tr.t("status.daemon_idle").to_string()
                    } else {
                        self.daemon_status.clone()
                    };
                    ui.weak(self.tr.f(
                        "status.every",
                        &[
                            ("s", &daemon_line),
                            ("n", &DAEMON_INTERVAL.as_secs().to_string()),
                        ],
                    ))
                    .on_hover_text(self.tr.t("status.daemon_idle_hover"));
                });
            });
            if self.export_rx.is_some() {
                ui.horizontal(|ui| {
                    if ui
                        .button(format!(
                            "{} {}",
                            icons::X,
                            self.tr.t("status.cancel_export")
                        ))
                        .on_hover_text(self.tr.t("status.cancel_export_hover"))
                        .clicked()
                    {
                        self.on_cancel_export();
                    }
                    let frac = if self.export_total > 0 {
                        self.export_rows as f32 / self.export_total as f32
                    } else {
                        0.0
                    };
                    ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0)).text(self.tr.f(
                        "status.exporting_bar",
                        &[
                            ("done", &self.export_rows.to_string()),
                            ("total", &self.export_total.to_string()),
                        ],
                    )));
                });
            }
            if self.scanning {
                ui.add(egui::ProgressBar::new(0.0).animate(true));
            }
        });

        if self.show_preview {
            egui::SidePanel::right("preview")
                .resizable(true)
                .default_width(340.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(icons::EYE).color(ACCENT).size(16.0));
                        ui.heading(self.tr.t("preview.title"));
                    });
                    let selected_entry = self.selected.and_then(|i| self.results.get(i).cloned());
                    match selected_entry {
                        None => {
                            ui.weak(self.tr.t("preview.empty"));
                        }
                        Some(entry) => {
                            ui.label(
                                self.tr
                                    .f("preview.path", &[("p", &entry.path.display().to_string())]),
                            );
                            ui.label(self.tr.f(
                                "preview.meta",
                                &[
                                    ("size", &human_size(entry.size)),
                                    ("full", &mtime_display(entry.mtime)),
                                    (
                                        "kind",
                                        &if entry.is_dir {
                                            self.tr.t("preview.kind_dir")
                                        } else {
                                            self.tr.t("preview.kind_file")
                                        },
                                    ),
                                ],
                            ));
                            ui.separator();
                            // Refresh the cached snippet only when the path,
                            // content needle, or case flag changes, so scrolling
                            // never re-reads the file but filter edits do.
                            let needle = self.content.trim().to_string();
                            let ci = self.case_insensitive;
                            if self.preview_path != entry.path
                                || self.preview_needle != needle
                                || self.preview_ci != ci
                            {
                                self.preview_path = entry.path.clone();
                                self.preview_needle = needle.clone();
                                self.preview_ci = ci;
                                let has_content = !needle.is_empty();
                                self.preview_text = content_snippet(
                                    &entry,
                                    has_content.then_some(needle.as_str()),
                                    ci,
                                );
                            }
                            // Centered pair: the row is the preview's action
                            // bar, not just another left-aligned line.
                            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                                ui.horizontal(|ui| {
                                    if ui
                                        .button(format!(
                                            "{} {}",
                                            icons::ARROW_SQUARE_OUT,
                                            self.tr.t("preview.open")
                                        ))
                                        .on_hover_text(self.tr.t("preview.open_hover"))
                                        .clicked()
                                    {
                                        self.on_open_selected();
                                    }
                                    if ui
                                        .button(format!(
                                            "{} {}",
                                            icons::FOLDER,
                                            self.tr.t("preview.reveal")
                                        ))
                                        .on_hover_text(self.tr.t("preview.reveal_hover"))
                                        .clicked()
                                    {
                                        self.on_reveal_selected();
                                    }
                                });
                            });
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                let ranges = highlight_ranges(
                                    &self.preview_text,
                                    needle.trim(),
                                    self.case_insensitive,
                                );
                                if ranges.is_empty() {
                                    ui.monospace(&self.preview_text);
                                } else {
                                    ui.add(egui::Label::new(highlight_job(
                                        &self.preview_text,
                                        &ranges,
                                        ui.visuals().text_color(),
                                        egui::FontId::monospace(14.0),
                                    )));
                                }
                            });
                        }
                    }
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 4.0);
            // Per-tab folders: this block belongs to the active tab
            // and its content switches with it (nothing here is
            // global). Chips remove, drives toggle, Add appends, and
            // the text field stays editable for pasting.
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(icons::FOLDER_OPEN)
                        .color(ACCENT)
                        .size(18.0),
                );
                ui.label(self.tr.t("folders.title"));
                let roots: Vec<String> = parse_roots(&self.root)
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect();
                let mut drop: Option<String> = None;
                if roots.is_empty() {
                    ui.weak(self.tr.t("folders.none"));
                }
                for r in &roots {
                    ui.label(egui::RichText::new(r).strong());
                    if ui
                        .small_button("x")
                        .on_hover_text(self.tr.f("folders.remove_hover", &[("r", r)]))
                        .clicked()
                    {
                        drop = Some(r.clone());
                    }
                }
                if ui
                    .small_button(format!("{} {}", icons::PLUS, self.tr.t("folders.add")))
                    .on_hover_text(self.tr.t("folders.add_hover"))
                    .clicked()
                {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.root = add_root(&self.root, &dir.display().to_string());
                    }
                }
                if let Some(d) = drop {
                    self.root = remove_root(&self.root, &d);
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(self.tr.t("folders.drives"));
                let drives: Vec<String> = self
                    .drives
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect();
                let mut toggle: Option<String> = None;
                for d in &drives {
                    let active = parse_roots(&self.root)
                        .iter()
                        .any(|p| roots_equal(&p.display().to_string(), d));
                    if ui
                        .selectable_label(active, d)
                        .on_hover_text(if active {
                            self.tr.f("folders.drive_on", &[("d", d)])
                        } else {
                            self.tr.f("folders.drive_off", &[("d", d)])
                        })
                        .clicked()
                    {
                        toggle = Some(d.clone());
                    }
                }
                if let Some(t) = toggle {
                    self.root = toggle_root(&self.root, &t);
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(self.tr.t("folders.edit"));
                let folders_hint = self.tr.t("folders.hint");
                ui.add(
                    egui::TextEdit::singleline(&mut self.root)
                        .desired_width(320.0)
                        .hint_text(folders_hint.as_str()),
                )
                .on_hover_text(self.tr.t("folders.hint_hover"));
            });
            // The tab's own run controls: a big Search as the primary
            // action, Refresh to re-run keeping the selection, and the
            // per-tab Live toggle.
            ui.horizontal_wrapped(|ui| {
                if self.scanning {
                    if ui
                        .button(format!("{} {}", icons::X, self.tr.t("run.cancel")))
                        .on_hover_text(self.tr.t("run.cancel_hover"))
                        .clicked()
                    {
                        self.on_cancel();
                    }
                    ui.add(egui::Spinner::new().size(16.0));
                } else {
                    let label = if self.use_indexed {
                        self.tr.t("run.search_index")
                    } else {
                        self.tr.t("run.scan")
                    };
                    let search = egui::Button::new(
                        egui::RichText::new(format!("{} {label}", icons::MAGNIFYING_GLASS))
                            .strong()
                            .size(20.0)
                            .color(egui::Color32::from_rgb(26, 20, 8)),
                    )
                    .fill(ACCENT);
                    if ui
                        .add_sized(egui::vec2(220.0, 44.0), search)
                        .on_hover_text(self.tr.t("run.search_hover"))
                        .clicked()
                    {
                        self.on_search();
                    }
                }
                // Manual refresh: re-runs the tab as-is and keeps the
                // selected file when it survives (page follows it).
                if ui
                    .button(format!(
                        "{} {}",
                        icons::ARROW_CLOCKWISE,
                        self.tr.t("run.refresh")
                    ))
                    .on_hover_text(self.tr.t("run.refresh_hover"))
                    .clicked()
                {
                    let sel = self
                        .selected
                        .and_then(|i| self.results.get(i).map(|e| e.path.clone()));
                    self.restore_select = sel;
                    self.on_search();
                }
                let live_label = self.tr.t("run.live");
                ui.checkbox(&mut self.live, live_label.as_str())
                    .on_hover_text(self.tr.t("run.live_hover"));
            });
            ui.separator();
            // Wrapped: narrow windows fold the export buttons onto
            // their own line instead of clipping them.
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(icons::LIST).color(ACCENT).size(18.0));
                ui.heading(
                    self.tr
                        .f("results.title", &[("n", &self.results.len().to_string())]),
                );
                if self.results.is_empty() {
                    return;
                }
                egui::ComboBox::from_id_salt("csv_delim")
                    .selected_text(self.tr.t(match self.csv_delim {
                        ';' => "results.delim_semicolon",
                        '\t' => "results.delim_tab",
                        '|' => "results.delim_pipe",
                        _ => "results.delim_comma",
                    }))
                    .show_ui(ui, |ui| {
                        for (delim, name) in [
                            (',', self.tr.t("results.delim_comma")),
                            (';', self.tr.t("results.delim_semicolon")),
                            ('\t', self.tr.t("results.delim_tab")),
                            ('|', self.tr.t("results.delim_pipe")),
                        ] {
                            ui.selectable_value(&mut self.csv_delim, delim, name);
                        }
                    })
                    .response
                    .on_hover_text(self.tr.t("results.delim_hover"));
                if ui
                    .button(format!("{} CSV", icons::FILE_CSV))
                    .on_hover_text(self.tr.t("results.export_csv_hover"))
                    .clicked()
                {
                    self.on_export_csv();
                }
                if ui
                    .button(format!("{} TXT", icons::FILE_TEXT))
                    .on_hover_text(self.tr.t("results.export_txt_hover"))
                    .clicked()
                {
                    self.on_export("Plain text", "txt", ExportFormat::Txt);
                }
                if ui
                    .button(format!("{} HTML", icons::FILE_HTML))
                    .on_hover_text(self.tr.t("results.export_html_hover"))
                    .clicked()
                {
                    self.on_export("HTML", "html", ExportFormat::Html);
                }
                if ui
                    .button(format!("{} Excel", icons::TABLE))
                    .on_hover_text(self.tr.t("results.export_excel_hover"))
                    .clicked()
                {
                    self.on_export("Excel XML", "xml", ExportFormat::ExcelXml);
                }
                if ui
                    .button(format!("{} RTF", icons::FILE_DOC))
                    .on_hover_text(self.tr.t("results.export_rtf_hover"))
                    .clicked()
                {
                    self.on_export("RTF", "rtf", ExportFormat::Rtf);
                }
                if ui
                    .button(format!("{} {}", icons::PRINTER, self.tr.t("results.print")))
                    .on_hover_text(self.tr.t("results.print_hover"))
                    .clicked()
                {
                    self.on_print();
                }
            });
            if self.results.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(60.0);
                    if self.scanning {
                        ui.add(egui::Spinner::new().size(32.0));
                        ui.heading(self.tr.t("results.empty_searching"));
                        ui.weak(self.tr.t("results.empty_searching_1"));
                    } else if !self.searched {
                        ui.label(
                            egui::RichText::new(icons::MAGNIFYING_GLASS)
                                .size(48.0)
                                .color(ACCENT),
                        );
                        ui.heading(self.tr.t("results.empty_never"));
                        ui.weak(self.tr.t("results.empty_never_1"));
                        ui.weak(self.tr.t("results.empty_never_2"));
                    } else {
                        ui.label(egui::RichText::new(icons::FILE_X).size(48.0).color(ACCENT));
                        ui.heading(self.tr.t("results.empty_nomatch"));
                        ui.weak(self.tr.t("results.empty_nomatch_1"));
                        ui.weak(self.tr.t("results.empty_nomatch_2"));
                    }
                });
                return;
            }
            ui.horizontal(|ui| {
                if ui
                    .button(format!(
                        "{} {}",
                        icons::CHECK_SQUARE,
                        self.tr.t("results.select_all")
                    ))
                    .on_hover_text(self.tr.t("results.select_all_hover"))
                    .clicked()
                {
                    self.checked = self.results.iter().map(|e| e.path.clone()).collect();
                }
                if ui
                    .button(format!("{} {}", icons::X, self.tr.t("results.select_none")))
                    .on_hover_text(self.tr.t("results.select_none_hover"))
                    .clicked()
                {
                    self.checked.clear();
                }
                ui.label(self.tr.f(
                    "results.selected",
                    &[("n", &self.checked.len().to_string())],
                ));
                if ui
                    .button(format!("{} {}", icons::COPY, self.tr.t("results.copy")))
                    .on_hover_text(self.tr.t("results.copy_hover"))
                    .clicked()
                {
                    self.on_copy_checked();
                }
                if ui
                    .button(format!("{} {}", icons::TRUCK, self.tr.t("results.move")))
                    .on_hover_text(self.tr.t("results.move_hover"))
                    .clicked()
                {
                    self.on_move_checked();
                }
                if ui
                    .button(format!("{} {}", icons::TRASH, self.tr.t("results.delete")))
                    .on_hover_text(self.tr.t("results.delete_hover"))
                    .clicked()
                {
                    if self.checked.is_empty() {
                        self.status = self.tr.t("status.nothing_checked").to_string();
                    } else {
                        self.confirm_delete = true;
                        self.modal_focus_pending = true;
                    }
                }
            });
            if self.confirm_delete {
                // True modal: dims the background and blocks clicks
                // behind it, so the dialog can never be lost.
                egui::Modal::new(egui::Id::new("confirm_delete")).show(ctx, |ui| {
                    ui.set_min_width(340.0);
                    ui.heading(format!(
                        "{} {}",
                        icons::TRASH,
                        self.tr.t("modal.delete_title")
                    ));
                    ui.label(self.tr.f(
                        "modal.delete_body",
                        &[("n", &self.checked.len().to_string())],
                    ));
                    // Sample what is checked so the click is never blind.
                    let shown: Vec<PathBuf> = self.checked.iter().take(5).cloned().collect();
                    for path in &shown {
                        ui.weak(path.display().to_string());
                    }
                    if self.checked.len() > shown.len() {
                        ui.weak(self.tr.f(
                            "modal.delete_more",
                            &[("n", &(self.checked.len() - shown.len()).to_string())],
                        ));
                    }
                    ui.horizontal(|ui| {
                        let danger = egui::Button::new(
                            egui::RichText::new(self.tr.t("modal.delete_btn"))
                                .color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(170, 40, 40));
                        if ui.add(danger).clicked() {
                            self.on_delete_checked();
                        }
                        let cancel = ui.button(self.tr.t("modal.cancel"));
                        if self.modal_focus_pending {
                            // Safe choice focused by default, once.
                            cancel.request_focus();
                            self.modal_focus_pending = false;
                        }
                        if cancel.clicked() {
                            self.confirm_delete = false;
                        }
                    });
                });
            }
            if self.confirm_move {
                let dest = self.pending_move.clone().unwrap_or_default();
                egui::Modal::new(egui::Id::new("confirm_move")).show(ctx, |ui| {
                    ui.set_min_width(340.0);
                    ui.label(self.tr.f(
                        "modal.move_body",
                        &[
                            ("n", &self.checked.len().to_string()),
                            ("dest", &dest.display().to_string()),
                        ],
                    ));
                    ui.horizontal(|ui| {
                        if ui.button(self.tr.t("modal.move_btn")).clicked() {
                            self.confirm_move = false;
                            if let Some(d) = self.pending_move.take() {
                                let (moved, skipped) = self.move_checked_to(&d);
                                self.status = self.tr.f(
                                    "status.moved",
                                    &[
                                        ("moved", &moved.to_string()),
                                        ("skipped", &skipped.to_string()),
                                        ("dest", &d.display().to_string()),
                                    ],
                                );
                            }
                        }
                        let cancel = ui.button(self.tr.t("modal.cancel"));
                        if self.modal_focus_pending {
                            // Safe choice focused by default, once.
                            cancel.request_focus();
                            self.modal_focus_pending = false;
                        }
                        if cancel.clicked() {
                            self.confirm_move = false;
                            self.pending_move = None;
                        }
                    });
                });
            }
            // Paginated controls: only the current page is ever rendered,
            // so 100k+ hit lists stay interactive.
            let pages = self.page_count();
            self.clamp_page();
            ui.horizontal(|ui| {
                if ui
                    .button(format!(
                        "{} {}",
                        icons::CARET_LEFT,
                        self.tr.t("results.prev")
                    ))
                    .on_hover_text(self.tr.t("results.prev_hover"))
                    .clicked()
                    && self.page > 0
                {
                    self.page -= 1;
                    self.selected = None;
                }
                let start = self.page * self.page_size;
                let end = (start + self.page_size).min(self.results.len());
                ui.label(self.tr.f(
                    "results.page",
                    &[
                        ("p", &(self.page + 1).to_string()),
                        ("pages", &pages.to_string()),
                        ("a", &(start + 1).to_string()),
                        ("b", &end.to_string()),
                        ("n", &self.results.len().to_string()),
                    ],
                ));
                if ui
                    .button(format!(
                        "{} {}",
                        icons::CARET_RIGHT,
                        self.tr.t("results.next")
                    ))
                    .on_hover_text(self.tr.t("results.next_hover"))
                    .clicked()
                    && self.page + 1 < pages
                {
                    self.page += 1;
                    self.selected = None;
                }
                ui.label(self.tr.t("results.go"));
                let mut jump = self.page + 1;
                if ui
                    .add(egui::DragValue::new(&mut jump).range(1..=pages.max(1)))
                    .on_hover_text(self.tr.t("results.go_hover"))
                    .changed()
                {
                    self.page = (jump - 1).min(pages.saturating_sub(1));
                    self.selected = None;
                }
                ui.label(format!("{}:", self.tr.t("results.page_size")));
                let before = self.page_size;
                egui::ComboBox::from_id_salt("page_size")
                    .selected_text(self.page_size.to_string())
                    .show_ui(ui, |ui| {
                        for s in PAGE_SIZES {
                            ui.selectable_value(&mut self.page_size, s, s.to_string());
                        }
                    })
                    .response
                    .on_hover_text(self.tr.t("results.page_size_hover"));
                if self.page_size != before {
                    self.clamp_page();
                    self.selected = None;
                }
            });
            let start = self.page * self.page_size;
            let end = (start + self.page_size).min(self.results.len());
            let path_arrow = self.sort_arrow(SortKey::Path).to_string();
            let size_arrow = self.sort_arrow(SortKey::Size).to_string();
            let mod_arrow = self.sort_arrow(SortKey::Modified).to_string();
            let mut sort_clicked: Option<SortKey> = None;
            let mut clicked_row: Option<usize> = None;
            let mut open_row: Option<usize> = None;
            let mut toggled_check: Option<(PathBuf, bool)> = None;
            let pending_action = std::cell::Cell::new(None);
            let open_label = self.tr.t("results.row_open");
            let open_hover = self.tr.t("results.row_open_hover");
            let reveal_label = self.tr.t("results.row_reveal");
            let reveal_hover = self.tr.t("results.row_reveal_hover");
            let props_label = self.tr.t("results.row_props");
            let props_hover = self.tr.t("results.row_props_hover");
            let rowcheck_hover = self.tr.t("results.rowcheck_hover");
            let path_header = self.tr.f("results.header_path", &[("arrow", &path_arrow)]);
            let size_header = self.tr.f("results.header_size", &[("arrow", &size_arrow)]);
            let mod_header = self.tr.f("results.header_mod", &[("arrow", &mod_arrow)]);
            let sort_path_hover = self.tr.t("results.sort_path");
            let sort_size_hover = self.tr.t("results.sort_size");
            let sort_mod_hover = self.tr.t("results.sort_mod");
            let name_needle = self.name.trim().to_string();
            let hl_ci = self.case_insensitive;
            let hl_normal = ui.visuals().text_color();
            let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
            let table_h = ui.available_height();
            // Shared click/right-click/double-click wiring for the
            // path, size, and date cells: click selects, right-click
            // selects and opens the menu, double-click opens.
            let mut wire_row_cell = |resp: egui::Response, gi: usize, hover: Option<&str>| {
                let resp = match hover {
                    Some(h) => resp.on_hover_text(h),
                    None => resp,
                };
                if resp.clicked() {
                    clicked_row = Some(gi);
                }
                if resp.secondary_clicked() {
                    clicked_row = Some(gi);
                }
                if resp.double_clicked() {
                    open_row = Some(gi);
                }
                resp.context_menu(|ui| {
                    if ui.button(&open_label).on_hover_text(&open_hover).clicked() {
                        pending_action.set(Some((gi, RowAction::Open)));
                        ui.close();
                    }
                    if ui
                        .button(&reveal_label)
                        .on_hover_text(&reveal_hover)
                        .clicked()
                    {
                        pending_action.set(Some((gi, RowAction::Reveal)));
                        ui.close();
                    }
                    if ui
                        .button(&props_label)
                        .on_hover_text(&props_hover)
                        .clicked()
                    {
                        pending_action.set(Some((gi, RowAction::Props)));
                        ui.close();
                    }
                });
            };
            // The header lives outside the scrolling body, so it stays
            // pinned while rows scroll; one column layout keeps headers
            // and cells aligned.
            TableBuilder::new(ui)
                .id_salt("results")
                .sense(egui::Sense::click())
                .striped(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(28.0))
                .column(Column::exact(28.0))
                .column(Column::remainder().clip(true))
                .column(Column::initial(90.0).resizable(true))
                .column(Column::initial(110.0).resizable(true))
                .max_scroll_height(table_h)
                .header(row_h, |mut header| {
                    header.col(|ui| {
                        ui.weak(icons::CHECK_SQUARE);
                    });
                    header.col(|ui| {
                        ui.weak("");
                    });
                    // Small buttons: full-size ones crowd the header
                    // and steal emphasis from the rows.
                    header.col(|ui| {
                        if ui
                            .small_button(path_header)
                            .on_hover_text(sort_path_hover)
                            .clicked()
                        {
                            sort_clicked = Some(SortKey::Path);
                        }
                    });
                    header.col(|ui| {
                        if ui
                            .small_button(size_header)
                            .on_hover_text(sort_size_hover)
                            .clicked()
                        {
                            sort_clicked = Some(SortKey::Size);
                        }
                    });
                    header.col(|ui| {
                        if ui
                            .small_button(mod_header)
                            .on_hover_text(sort_mod_hover)
                            .clicked()
                        {
                            sort_clicked = Some(SortKey::Modified);
                        }
                    });
                })
                .body(|body| {
                    body.rows(row_h, end - start, |mut row| {
                        let gi = start + row.index();
                        let e = &self.results[gi];
                        let path_str = e.path.display().to_string();
                        let size_str = human_size(e.size);
                        let mod_str = mtime_short(e.mtime);
                        let mod_full = mtime_display(e.mtime);
                        let is_sel = self.selected == Some(gi);
                        row.set_selected(is_sel);
                        row.col(|ui| {
                            let mut is_checked = self.checked.contains(&e.path);
                            if ui
                                .checkbox(&mut is_checked, "")
                                .on_hover_text(&rowcheck_hover)
                                .clicked()
                            {
                                toggled_check = Some((e.path.clone(), is_checked));
                            }
                        });
                        row.col(|ui| {
                            ui.label(egui::RichText::new(file_icon(e)).color(if e.is_dir {
                                ACCENT
                            } else {
                                ui.visuals().weak_text_color()
                            }));
                        });
                        let ranges = highlight_ranges(&path_str, &name_needle, hl_ci);
                        // Cells leave clicks to the table's full-row target.
                        row.col(|ui| {
                            let path_resp = if is_sel || ranges.is_empty() {
                                ui.add(egui::Label::new(&path_str).selectable(false))
                            } else {
                                let job = highlight_job(
                                    &path_str,
                                    &ranges,
                                    hl_normal,
                                    egui::FontId::proportional(14.0),
                                );
                                ui.add(egui::Label::new(job).selectable(false))
                            };
                            // Full path as tooltip: the column clips long
                            // paths, and hovering used to show nothing.
                            path_resp.on_hover_text(&path_str);
                        });
                        row.col(|ui| {
                            ui.add(egui::Label::new(size_str).selectable(false));
                        });
                        // Short date in the column; the full stamp shows
                        // on hover.
                        row.col(|ui| {
                            let date_resp = ui.add(egui::Label::new(mod_str).selectable(false));
                            date_resp.on_hover_text(&mod_full);
                        });
                        wire_row_cell(row.response(), gi, None);
                    });
                });
            if let Some((path, now)) = toggled_check {
                if now {
                    self.checked.insert(path);
                } else {
                    self.checked.remove(&path);
                }
            }
            if let Some(key) = sort_clicked {
                self.set_sort(key);
            }
            if let Some(gi) = clicked_row {
                self.selected = Some(gi);
            }
            // Right-click menu actions land on the row the menu opened
            // on (right-click always selects it first).
            if let Some((gi, action)) = pending_action.get() {
                self.selected = Some(gi);
                match action {
                    RowAction::Open => self.on_open_selected(),
                    RowAction::Reveal => self.on_reveal_selected(),
                    RowAction::Props => self.on_props_selected(),
                }
            }
            if let Some(gi) = open_row {
                self.selected = Some(gi);
                self.on_open_selected();
            }
        });
    }
}

/// Folder passed on the command line (`--folder <dir>` or a bare
/// directory argv, as sent by the Explorer context menu). Returns the
/// first argv that names an existing directory.
fn initial_folder_from_args(args: &[String]) -> Option<PathBuf> {
    let mut iter = args.iter().skip(1);
    while let Some(a) = iter.next() {
        if a == "--folder" {
            // Explicit flag wins: a missing/bad dir means "no folder".
            return iter
                .next()
                .filter(|p| PathBuf::from(p).is_dir())
                .map(PathBuf::from);
        }
        if !a.starts_with('-') && PathBuf::from(a).is_dir() {
            return Some(PathBuf::from(a));
        }
    }
    None
}

fn main() -> eframe::Result<()> {
    // Load first so the window opens with the remembered geometry.
    let mut app = App::new();
    // Context-menu / launcher handoff: start rooted at the folder the
    // user right-clicked instead of the remembered roots.
    if let Some(dir) = initial_folder_from_args(&std::env::args().collect::<Vec<_>>()) {
        app.set_initial_root(dir);
    }
    let mut options = eframe::NativeOptions::default();
    options.viewport = egui::ViewportBuilder::default()
        .with_inner_size([app.config.window.width, app.config.window.height])
        .with_maximized(app.config.window.maximized);
    match eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")) {
        Ok(icon) => options.viewport = options.viewport.with_icon(icon),
        Err(e) => eprintln!("window icon missing, continuing without it: {e}"),
    }
    eframe::run_native(
        "SuperSearch",
        options,
        Box::new(|cc| {
            // Phosphor icons as a proportional fallback: glyph constants
            // like `icons::TRASH` render inline in any label or button.
            let mut fonts = egui::FontDefinitions::default();
            egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
            install_system_fallbacks(&mut fonts);
            cc.egui_ctx.set_fonts(fonts);
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_table_renders_and_right_click_selects_row_headlessly() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::default();
        app.config_path = tmp.path().join("config.json");
        app.results = vec![
            FileEntry::new("example-first.txt".into(), 10, 1_700_000_000, false),
            FileEntry::new("example-second.txt".into(), 20, 1_700_000_000, false),
        ];
        let ctx = egui::Context::default();
        let mut frame = eframe::Frame::_new_kittest();
        let mut render = |events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1600.0, 1600.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| eframe::App::update(&mut app, ctx, &mut frame),
            )
        };
        render(vec![]);
        let output = render(vec![]);
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape {
                    if text.galley.text() == "example-second.txt" {
                        return Some(text.pos + egui::vec2(5.0, 5.0));
                    }
                }
                None
            })
            .expect("second result rendered in the table");
        render(vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed: true,
                modifiers: Default::default(),
            },
        ]);
        render(vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed: false,
            modifiers: Default::default(),
        }]);
        assert_eq!(app.selected, Some(1));
    }

    /// The indexer toggle is headless state machine (no `egui::Context`
    /// needed): validation messages, start, status refresh, stop.
    /// `config_path` is redirected first so `persist()` never touches
    /// the real user config.
    #[test]
    fn daemon_toggle_validates_starts_and_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::default();
        app.config_path = tmp.path().join("cfg.json");
        app.root.clear();
        app.db_path.clear();
        app.on_daemon_toggle();
        assert!(app.daemon.is_none());
        assert!(app.daemon_status.contains("folder"));

        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        app.root = root.display().to_string();
        app.on_daemon_toggle();
        assert!(app.daemon.is_none());
        assert!(app.daemon_status.contains("DB path"));

        app.db_path = tmp.path().join("idx.db").display().to_string();
        app.on_daemon_toggle();
        assert!(app.daemon.is_some());
        assert_eq!(app.daemon_status, "Indexer started.");

        // Status refresh reads only atomics/locks: safe headless.
        app.poll_daemon_status();
        assert!(app.daemon_status.starts_with("Indexer: last indexed"));

        app.on_daemon_toggle();
        assert!(app.daemon.is_none());
        assert_eq!(app.daemon_status, "Indexer stopped.");
    }

    #[test]
    fn highlight_ranges_find_all_case_variants() {
        assert!(highlight_ranges("abc", "", false).is_empty());
        assert_eq!(highlight_ranges("aXbXc", "X", false), vec![(1, 2), (3, 4)]);
        assert!(highlight_ranges("aXbXc", "x", false).is_empty());
        assert_eq!(highlight_ranges("aXb", "x", true), vec![(1, 2)]);
        // Non-ASCII falls back to a sensitive search, never panics.
        assert!(highlight_ranges("Äpfel Äpfel", "äpfel", true).is_empty());
        assert_eq!(
            highlight_ranges("Äpfel Äpfel", "Äpfel", true),
            vec![(0, 6), (7, 13)]
        );
        // Garbage ranges never panic the job builder.
        let job = highlight_job(
            "hi",
            &[(0, 99), (50, 51)],
            egui::Color32::WHITE,
            egui::FontId::default(),
        );
        assert!(!job.sections.is_empty());
        let empty = highlight_job("hi", &[], egui::Color32::WHITE, egui::FontId::default());
        assert_eq!(empty.sections.len(), 1);
    }

    #[test]
    fn filter_key_tracks_filters_and_sources() {
        let mut app = App::default();
        app.config_path = std::env::temp_dir().join("ss-test-no-write.json");
        // Hermetic baseline: `App::new` loads the ambient user config,
        // whose roots/DB may coincide with the flip targets below.
        app.root = "Z:\\base".to_string();
        app.db_path.clear();
        let base = app.filter_key();
        // Sources flip the question too, so they re-fire live search.
        app.root = "C:\\".to_string();
        assert_ne!(app.filter_key(), base);
        app.db_path = "x.db".to_string();
        let sourced = app.filter_key();
        app.use_indexed = true;
        assert_ne!(app.filter_key(), sourced);
        app.name = "report".to_string();
        let named = app.filter_key();
        assert_ne!(named, sourced);
        app.case_insensitive = true;
        assert_ne!(app.filter_key(), named);
        app.case_insensitive = false;
        assert_eq!(app.filter_key(), named);
    }

    #[test]
    fn file_icons_follow_type_and_folders_win() {
        let dir = FileEntry::new(PathBuf::from(r"C:\d"), 0, 0, true);
        assert_eq!(file_icon(&dir), icons::FOLDER);
        let img = FileEntry::new(PathBuf::from(r"C:\a.PNG"), 1, 0, false);
        assert_eq!(file_icon(&img), icons::FILE_IMAGE);
        let code = FileEntry::new(PathBuf::from(r"C:\m.rs"), 1, 0, false);
        assert_eq!(file_icon(&code), icons::FILE_CODE);
        let zip = FileEntry::new(PathBuf::from(r"C:\a.zip"), 1, 0, false);
        assert_eq!(file_icon(&zip), icons::FILE_ZIP);
        let plain = FileEntry::new(PathBuf::from(r"C:\a.xyz"), 1, 0, false);
        assert_eq!(file_icon(&plain), icons::FILE);
    }

    #[test]
    fn human_sizes_use_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1_048_576), "1.0 MB");
        assert_eq!(human_size(2_147_483_648), "2.0 GB");
    }

    #[test]
    fn dates_render_ymd_and_clamp_future() {
        assert_eq!(ymd_from_days(0), (1970, 1, 1));
        assert_eq!(ymd_from_days(-1), (1969, 12, 31));
        assert_eq!(ymd_from_days(19723), (2024, 1, 1));
        // 1700000000 = 2023-11-14 22:13 UTC.
        assert_eq!(ymd_from_days(1700000000 / 86_400), (2023, 11, 14));
        let rendered = mtime_display(1700000000);
        assert!(rendered.starts_with("2023-11-14 22:13"), "was: {rendered}");
        // Future stamps clamp instead of rendering negative ages.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let future = mtime_display(now + 30 * 86_400);
        assert!(future.ends_with("(0d ago)"), "was: {future}");
    }

    #[test]
    fn worker_sort_matches_header_sort() {
        let rows = || {
            vec![
                FileEntry::new(PathBuf::from(r"C:\b.txt"), 20, 5, false),
                FileEntry::new(PathBuf::from(r"C:\a.txt"), 10, 9, false),
                FileEntry::new(PathBuf::from(r"C:\c.txt"), 15, 1, false),
            ]
        };
        let mut via_worker = rows();
        sort_entries(&mut via_worker, SortKey::Size, false);
        let mut app = App::default();
        app.config_path = std::env::temp_dir().join("ss-test-no-write.json");
        app.results = rows();
        app.sort_key = SortKey::Size;
        app.sort_asc = false;
        app.sort_results();
        assert_eq!(via_worker, app.results);
        // Descending size: b(20), c(15), a(10).
        assert_eq!(via_worker[0].size, 20);
        assert_eq!(via_worker[2].size, 10);
    }

    #[test]
    fn clear_filters_resets_inputs_but_keeps_source() {
        let mut app = App::default();
        app.config_path = std::env::temp_dir().join("ss-test-no-write.json");
        app.name = "x".to_string();
        app.regex = "y".to_string();
        app.min_size = "10".to_string();
        app.exclusions = "target".to_string();
        app.case_insensitive = true;
        app.root = "C:\\".to_string();
        app.clear_filters();
        assert!(app.name.is_empty());
        assert!(app.regex.is_empty());
        assert!(app.min_size.is_empty());
        assert!(app.exclusions.is_empty());
        assert!(!app.case_insensitive);
        assert_eq!(app.root, "C:\\");
    }

    #[test]
    fn checked_paths_follow_selection_order() {
        let mut app = App::default();
        app.config_path = std::env::temp_dir().join("ss-test-no-write.json");
        app.results = vec![
            FileEntry::new(PathBuf::from(r"C:\b.txt"), 1, 0, false),
            FileEntry::new(PathBuf::from(r"C:\a.txt"), 2, 0, false),
        ];
        assert!(app.checked_paths().is_empty());
        app.checked.insert(PathBuf::from(r"C:\a.txt"));
        app.checked.insert(PathBuf::from(r"C:\gone.txt"));
        let paths = app.checked_paths();
        assert_eq!(paths, vec![PathBuf::from(r"C:\a.txt")]);
        // Selection survives sorts and refines: paths, not positions.
        app.results.swap(0, 1);
        assert_eq!(app.checked_paths(), vec![PathBuf::from(r"C:\a.txt")]);
    }

    #[test]
    fn checked_paths_come_back_in_result_order() {
        let mut app = App::default();
        app.config_path = std::env::temp_dir().join("ss-test-no-write.json");
        let a = PathBuf::from(r"C:\a.txt");
        let b = PathBuf::from(r"C:\b.txt");
        let c = PathBuf::from(r"C:\c.txt");
        app.results = vec![
            FileEntry::new(b.clone(), 1, 0, false),
            FileEntry::new(a.clone(), 2, 0, false),
            FileEntry::new(c.clone(), 3, 0, false),
        ];
        // Insert newest-first plus a stale path: result order wins,
        // and paths missing from the results never leak through.
        app.checked.insert(c.clone());
        app.checked.insert(a.clone());
        app.checked.insert(PathBuf::from(r"C:\gone.txt"));
        assert_eq!(app.checked_paths(), vec![a, c]);
    }

    /// `persist` must carry the delimiter picker and the exclusion
    /// field into the config file (both are restored on next launch).
    /// `config_path` is redirected first so the real user config is
    /// never touched.
    #[test]
    fn persist_roundtrips_csv_delim_and_exclusions() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::default();
        app.config_path = tmp.path().join("cfg.json");
        app.root = String::new();
        app.db_path = String::new();
        app.csv_delim = ';';
        app.exclusions = "target,  *.tmp ,, node_modules".to_string();
        app.persist();
        let back = AppConfig::load_from(&app.config_path).unwrap();
        assert_eq!(back.csv_delim, ';');
        assert_eq!(back.exclusions, vec!["target", "*.tmp", "node_modules"]);
    }

    /// Every filter control plus both source flags feeds the live-search
    /// fingerprint: flipping any one field must re-fire the debounce.
    /// Fields start from a blank baseline so the real user config on
    /// the test machine cannot hide a component.
    #[test]
    fn filter_key_tracks_every_filter_field() {
        let mut app = App::default();
        app.config_path = std::env::temp_dir().join("ss-test-no-write.json");
        app.root.clear();
        app.db_path.clear();
        app.name.clear();
        app.regex.clear();
        app.fuzzy.clear();
        app.content.clear();
        app.glob.clear();
        app.extensions.clear();
        app.min_size.clear();
        app.max_size.clear();
        app.modified_after.clear();
        app.modified_before.clear();
        app.exclusions.clear();
        app.case_insensitive = false;
        app.use_indexed = false;
        let base = app.filter_key();
        macro_rules! flip {
            ($field:ident, $value:expr) => {{
                app.$field = $value.to_string();
                assert_ne!(app.filter_key(), base);
                app.$field.clear();
                assert_eq!(app.filter_key(), base);
            }};
        }
        flip!(root, r"C:\r");
        flip!(db_path, "x.db");
        flip!(name, "report");
        flip!(regex, "a.*b");
        flip!(fuzzy, "mrs");
        flip!(content, "TODO");
        flip!(glob, "*.rs");
        flip!(extensions, "rs, md");
        flip!(min_size, "10");
        flip!(max_size, "100");
        flip!(modified_after, "2024-01-01");
        flip!(modified_before, "2024-02-01");
        flip!(exclusions, "target");
        flip!(or_terms, "a, b");
        flip!(not_terms, "c");
        app.show_hidden = true;
        assert_ne!(app.filter_key(), base);
        app.show_hidden = false;
        assert_eq!(app.filter_key(), base);
        app.case_insensitive = true;
        assert_ne!(app.filter_key(), base);
        app.case_insensitive = false;
        assert_eq!(app.filter_key(), base);
        app.use_indexed = true;
        assert_ne!(app.filter_key(), base);
        app.use_indexed = false;
        assert_eq!(app.filter_key(), base);
    }

    /// The regex helper gates query construction: an invalid pattern is
    /// a status-line error, never a silent empty table; blanks stay unset.
    #[test]
    fn current_query_rejects_invalid_regex() {
        let mut app = App::default();
        app.config_path = std::env::temp_dir().join("ss-test-no-write.json");
        app.regex = "(unclosed".to_string();
        let err = app.current_query().unwrap_err();
        assert!(err.contains("invalid regex"), "was: {err}");
        app.regex = "a.*b".to_string();
        assert_eq!(app.current_query().unwrap().regex.as_deref(), Some("a.*b"));
        app.regex = "   ".to_string();
        assert!(app.current_query().unwrap().regex.is_none());
    }

    /// The empty-selection guards in front of both confirm paths run
    /// headless (the folder picker and the modal itself need a display).
    #[test]
    fn confirm_paths_guard_empty_selection_headless() {
        let mut app = App::default();
        app.config_path = std::env::temp_dir().join("ss-test-no-write.json");
        app.on_move_checked();
        assert_eq!(app.status, "Nothing checked.");
        assert!(!app.confirm_move);
        assert!(!app.modal_focus_pending);
        app.confirm_delete = true;
        app.modal_focus_pending = true;
        app.on_delete_checked();
        assert!(!app.confirm_delete);
        assert_eq!(app.status, "Nothing checked.");
    }

    /// Exit removes the last print-view temp file so print views never
    /// accumulate. This is the headless-reachable half of the print
    /// lifecycle (`on_print` itself always opens a browser).
    #[test]
    fn exit_removes_last_print_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::default();
        app.config_path = tmp.path().join("cfg.json");
        let print = tmp.path().join("supersearch-print-1.html");
        std::fs::write(&print, "<html></html>").unwrap();
        app.last_print = Some(print.clone());
        eframe::App::on_exit(&mut app, None);
        assert!(!print.exists());
        assert!(app.last_print.is_none());
    }

    /// `initial_folder_from_args` honors `--folder <dir>` and bare
    /// directory argv (the context-menu handoff), ignoring flags and
    /// non-directories.
    #[test]
    fn initial_folder_from_args_prefers_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().display().to_string();
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            initial_folder_from_args(&argv(&["supersearch-gui", "--folder", &dir])),
            Some(tmp.path().to_path_buf())
        );
        assert_eq!(
            initial_folder_from_args(&argv(&["supersearch-gui", &dir])),
            Some(tmp.path().to_path_buf())
        );
        assert_eq!(
            initial_folder_from_args(&argv(&["supersearch-gui", "--folder"])),
            None
        );
        assert_eq!(
            initial_folder_from_args(&argv(&[
                "supersearch-gui",
                "--folder",
                r"Z:\definitely\missing",
                &dir
            ])),
            None
        );
        assert_eq!(
            initial_folder_from_args(&argv(&["supersearch-gui", "--live", r"C:\nope"])),
            None
        );
        // A handoff root replaces the remembered one and reseeds the
        // live-search key instead of marking the tab dirty.
        let mut app = App::default();
        app.config_path = tmp.path().join("cfg.json");
        app.set_initial_root(tmp.path().to_path_buf());
        assert_eq!(app.tabs[0].root, dir);
        assert_eq!(app.tabs[0].filter_key_cache, app.filter_key());
    }

    /// The context-menu toggle roundtrips: install, detect, remove
    /// (HKCU on Windows, user-level action files on Linux). Cleans up
    /// after itself so the machine is left as found.
    #[test]
    fn context_menu_roundtrips() {
        assert!(!App::context_menu_installed());
        App::set_context_menu(true).expect("context menu installs");
        assert!(App::context_menu_installed());
        App::set_context_menu(false).expect("context menu uninstalls");
        assert!(!App::context_menu_installed());
    }

    /// `;` splits roots (commas stay: extensions and exclusions use
    /// them as their own separator).
    #[test]
    fn parse_roots_splits_and_trims() {
        assert!(parse_roots("").is_empty());
        assert!(parse_roots("  ; ").is_empty());
        assert_eq!(
            parse_roots(r"C:\a; D:\b ;E:\c"),
            vec![
                PathBuf::from(r"C:\a"),
                PathBuf::from(r"D:\b"),
                PathBuf::from(r"E:\c")
            ]
        );
        assert_eq!(parse_roots(r"C:\a,b"), vec![PathBuf::from(r"C:\a,b")]);
    }

    /// Size fields take bare bytes plus KB/MB/GB/TB suffixes (binary,
    /// decimals allowed) — the same inputs the CLI accepts.
    #[test]
    fn parse_size_accepts_suffixes() {
        let tmpl = "invalid {flag} {got}: expected like 1048576 or 10MB";
        assert_eq!(parse_size("", "min size", tmpl).unwrap(), None);
        assert_eq!(parse_size("1024", "min size", tmpl).unwrap(), Some(1024));
        assert_eq!(
            parse_size("10M", "min size", tmpl).unwrap(),
            Some(10 * 1024 * 1024)
        );
        assert_eq!(parse_size("2 KB", "min size", tmpl).unwrap(), Some(2048));
        assert_eq!(
            parse_size("1.5g", "min size", tmpl).unwrap(),
            Some(1_610_612_736)
        );
        assert!(parse_size("abc", "min size", tmpl).is_err());
    }

    /// New tabs inherit the last-used folders, Live mode, and
    /// exclusions — but start with none of the old tab's filters or
    /// results.
    #[test]
    fn new_tab_inherits_last_used_folders() {
        let mut app = App::default();
        app.config_path = std::env::temp_dir().join("ss-test-no-write.json");
        app.root = r"C:\a; D:\b".to_string();
        app.live = true;
        app.exclusions = "target".to_string();
        app.name = "stale".to_string();
        app.new_tab();
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active, 1);
        let tab = app.tab();
        assert_eq!(tab.root, r"C:\a; D:\b");
        assert!(tab.live);
        assert_eq!(tab.exclusions, "target");
        assert!(tab.name.is_empty());
        assert!(!tab.searched);
        assert!(tab.results.is_empty());
    }

    /// The last tab can never be closed; closing an earlier tab keeps
    /// the active tab on the same search.
    #[test]
    fn close_tab_never_closes_the_last() {
        let mut app = App::default();
        app.config_path = std::env::temp_dir().join("ss-test-no-write.json");
        app.root = "first".to_string();
        app.close_tab(0);
        assert_eq!(app.tabs.len(), 1);
        app.new_tab();
        app.tabs[1].root = "second".to_string();
        app.close_tab(0);
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active, 0);
        assert_eq!(app.root, "second");
    }

    /// Saved snapshots round-trip the tab extras (roots, OR/NOT,
    /// hidden); legacy snapshots without roots keep the tab's current
    /// folders instead of wiping them.
    #[test]
    fn saved_search_roundtrips_tab_extras() {
        let mut app = App::default();
        app.config_path = std::env::temp_dir().join("ss-test-no-write.json");
        app.root = r"C:\a; D:\b".to_string();
        app.or_terms = "x, y".to_string();
        app.not_terms = "z".to_string();
        app.show_hidden = true;
        let saved = app.current_saved_search("s").unwrap();
        assert_eq!(saved.root, r"C:\a; D:\b");
        assert_eq!(saved.or_terms, vec!["x".to_string(), "y".to_string()]);
        assert_eq!(saved.not_terms, vec!["z".to_string()]);
        assert!(saved.show_hidden);

        app.root.clear();
        app.or_terms.clear();
        app.not_terms.clear();
        app.show_hidden = false;
        app.apply_saved_search(&saved);
        assert_eq!(app.root, r"C:\a; D:\b");
        assert_eq!(app.or_terms, "x, y");
        assert_eq!(app.not_terms, "z");
        assert!(app.show_hidden);

        let mut legacy = saved.clone();
        legacy.root.clear();
        app.root = "keep me".to_string();
        app.apply_saved_search(&legacy);
        assert_eq!(app.root, "keep me");
    }

    /// Missing folders never spawn a worker: the status names them,
    /// old rows stay, and the dirty dot survives (the search key is
    /// stamped at spawn, not at validation).
    #[test]
    fn on_search_rejects_missing_roots_headless() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::default();
        app.config_path = tmp.path().join("cfg.json");
        app.root = r"Z:\definitely\missing\xyz".to_string();
        // A filter is set so the missing-root check (not the
        // empty-filter guard) answers.
        app.name = "x".to_string();
        app.on_search();
        assert!(
            app.status.contains("None of the folders exist"),
            "was: {}",
            app.status
        );
        assert!(!app.scanning);
        assert!(app.rx.is_none());
        assert!(app.last_search_key.is_empty());

        app.root.clear();
        app.on_search();
        assert!(app.status.contains("Pick a folder"), "was: {}", app.status);
        assert!(!app.scanning);
    }

    /// An empty filter set never scans, live or explicit: the status
    /// says what to do instead and no worker spawns.
    #[test]
    fn on_search_refuses_empty_filters() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::default();
        app.config_path = tmp.path().join("cfg.json");
        app.root = tmp.path().display().to_string();
        app.on_search();
        assert!(!app.scanning);
        assert!(app.rx.is_none());
        assert!(
            app.status.contains("Type a filter first"),
            "was: {}",
            app.status
        );
        assert!(app.last_search_key.is_empty());
    }

    /// One good root among missing ones still starts a scan (headless:
    /// a tiny temp dir), and cancel cleans up without touching rows.
    #[test]
    fn on_search_starts_with_partial_roots_and_cancels() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::default();
        app.config_path = tmp.path().join("cfg.json");
        app.root = format!("{};Z:\\definitely\\missing", tmp.path().display());
        app.name = "x".to_string();
        app.on_search();
        assert!(app.scanning);
        assert_eq!(app.status, "Scanning…");
        assert!(!app.last_search_key.is_empty());
        app.on_cancel();
        assert!(!app.scanning);
        assert_eq!(app.status, "Cancelled.");
    }

    /// The hidden helper never hides unstatable paths (a stale index
    /// row still answers the query); plain files stay visible on every
    /// platform, dotfiles hide where the platform says so.
    #[test]
    fn hidden_helper_keeps_unstatable_paths() {
        assert!(!entry_hidden(Path::new(r"Z:\definitely\missing\xyz.txt")));
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain.txt");
        std::fs::write(&plain, "x").unwrap();
        assert!(!entry_hidden(&plain));
        #[cfg(not(windows))]
        {
            let dot = tmp.path().join(".hidden");
            std::fs::write(&dot, "x").unwrap();
            assert!(entry_hidden(&dot));
        }
    }

    /// Root chips and drive toggles only rewrite the `;`-separated
    /// field: add is idempotent, remove drops every equal spelling,
    /// toggle flips.
    #[test]
    fn root_field_surgery_adds_removes_toggles() {
        assert_eq!(add_root("", r"C:\a"), r"C:\a");
        assert_eq!(add_root(r"C:\a", r"C:\b"), r"C:\a; C:\b");
        // Idempotent, spelling-insensitive on Windows.
        assert_eq!(add_root(r"C:\a", r"C:\a"), r"C:\a");
        #[cfg(windows)]
        assert_eq!(add_root(r"C:\a", r"c:\A"), r"C:\a");
        assert_eq!(remove_root(r"C:\a; C:\b", r"C:\a"), r"C:\b");
        assert_eq!(remove_root(r"C:\a", r"C:\a"), "");
        #[cfg(windows)]
        assert_eq!(remove_root(r"C:\a; D:\", r"c:\A"), r"D:\");
        // Toggle flips membership and keeps the rest.
        assert_eq!(toggle_root("", r"C:\"), r"C:\");
        assert_eq!(toggle_root(r"C:\", r"C:\"), "");
        assert_eq!(toggle_root(r"C:\a; C:\b", r"C:\b"), r"C:\a");
        assert_eq!(toggle_root(r"C:\a", r"C:\b"), r"C:\a; C:\b");
    }

    /// Drive listing never panics and always offers something real
    /// (the system drive on Windows, `/` on Unix).
    #[test]
    fn drive_listing_offers_a_real_root() {
        let drives = list_drives();
        assert!(!drives.is_empty());
        #[cfg(windows)]
        assert!(drives.iter().any(|d| d == &PathBuf::from("C:\\")));
        #[cfg(not(windows))]
        assert!(drives.iter().any(|d| d == &PathBuf::from("/")));
    }

    /// Tab titles name the query and the locations: filter first,
    /// then first-root basename with a +N overflow; blank tabs keep
    /// the numbered fallback.
    #[test]
    fn tab_titles_name_query_and_locations() {
        let mut app = App::default();
        app.config_path = std::env::temp_dir().join("ss-test-no-write.json");
        // Hermetic blank baseline: `App::new` loads the ambient config.
        app.name.clear();
        app.root.clear();
        assert_eq!(app.tab_title(0), "Search 1");
        app.name = "report".to_string();
        app.root = r"C:\docs; D:\media".to_string();
        assert_eq!(app.tab_title(0), "report · docs +1");
        app.name.clear();
        app.glob = "*.rs".to_string();
        app.root = r"C:\".to_string();
        assert_eq!(app.tab_title(0), r"*.rs · C:\");
        app.glob.clear();
        app.or_terms = "x, y".to_string();
        app.root = r"C:\docs".to_string();
        assert_eq!(app.tab_title(0), "x, y · docs");
        app.or_terms.clear();
        app.name = "a-very-long-filter-name-here".to_string();
        assert_eq!(app.tab_title(0), "a-very-long-filter-n… · docs");
        app.name.clear();
        app.root.clear();
        assert_eq!(app.tab_title(0), "Search 1");
    }

    /// Short dates render bare `YYYY-MM-DD` (time-zone independent:
    /// whole days since the epoch).
    #[test]
    fn short_dates_render_ymd() {
        // 1700000000 = 2023-11-14 22:13 UTC.
        assert_eq!(mtime_short(1700000000), "2023-11-14");
        assert_eq!(mtime_short(0), "1970-01-01");
    }

    /// Refresh keeps the selected file when it survives the new run
    /// (page follows it) and restarts at the top otherwise — the
    /// headless half of the Refresh button + worker drain.
    #[test]
    fn refresh_restores_selection_and_page() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = App::default();
        app.config_path = tmp.path().join("cfg.json");
        let entries: Vec<FileEntry> = (0..250)
            .map(|i| FileEntry::new(PathBuf::from(format!("f{i:03}")), i as u64, 0, false))
            .collect();
        // Survivor at index 150 → page 1 (page size 100).
        let (tx, rx) = mpsc::channel();
        app.restore_select = Some(PathBuf::from("f150"));
        app.rx = Some(rx);
        app.scanning = true;
        tx.send(WorkerMsg::Done(entries.clone(), "ok".into()))
            .unwrap();
        app.poll_worker();
        assert_eq!(app.selected, Some(150));
        assert_eq!(app.page, 1);
        assert!(!app.scanning);
        assert_eq!(app.last_hits, 250);

        // Gone file → selection cleared, back to the top.
        let (tx, rx) = mpsc::channel();
        app.restore_select = Some(PathBuf::from("vanished"));
        app.rx = Some(rx);
        app.scanning = true;
        tx.send(WorkerMsg::Done(entries, "ok".into())).unwrap();
        app.poll_worker();
        assert_eq!(app.selected, None);
        assert_eq!(app.page, 0);
    }
}

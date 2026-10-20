//! Terminal UI state: tabs, filters, results, background search worker,
//! and every row action. Rendering lives in `ui.rs`; this module owns
//! the data and the key handling so both stay unit-testable without a
//! terminal.
//!
//! Feature notes (kept in sync with the desktop GUI):
//! - one search per tab: roots, filters, results, selection, sort;
//! - every GUI filter: name, regex, fuzzy, content, glob, OR/NOT,
//!   size/date bounds, exclusions, extensions, case, hidden, indexed;
//! - worker thread + progress + cancel (late results are dropped);
//! - row actions: open, reveal, properties, copy, move, delete, export;
//! - preview pane, saved searches, language switcher, help overlay.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use supersearch_config::{builtin_file_groups, AppConfig, SavedSearch};
use supersearch_core::export::{self, ExportFormat};
use supersearch_core::FileEntry;
use supersearch_i18n::{I18n, Lang};
use supersearch_index::{load_db, ScanOptions};
use supersearch_query::{parse_mtime, regex_is_valid, search, Query};

/// Rows per results page (the desktop GUI pages the same way so
/// 100k+ hit lists stay interactive).
pub const PAGE_SIZE: usize = 100;
/// Preview shows the first lines of the selected file.
const PREVIEW_LINES: usize = 40;
/// Bytes read for a preview (matches the query crate's content cap
/// scale so previews never load whole multi-GB files).
const PREVIEW_BYTES: u64 = 128 * 1024;

/// Sortable results column (mirrors the desktop header clicks).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Path,
    Size,
    Modified,
}

/// Editable filter fields, in Tab order. `DbPath` and `Roots` are
/// source selectors; the rest map one-to-one onto [`Query`] fields.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Roots,
    Name,
    Regex,
    Fuzzy,
    Content,
    Glob,
    Or,
    Not,
    MinSize,
    MaxSize,
    After,
    Before,
    Exclude,
    Ext,
    DbPath,
}

impl Field {
    /// All fields in Tab order.
    pub const ALL: [Field; 15] = [
        Field::Roots,
        Field::Name,
        Field::Regex,
        Field::Fuzzy,
        Field::Content,
        Field::Glob,
        Field::Or,
        Field::Not,
        Field::MinSize,
        Field::MaxSize,
        Field::After,
        Field::Before,
        Field::Exclude,
        Field::Ext,
        Field::DbPath,
    ];

    /// Translation key for the field label (reuses the desktop keys so
    /// all 130+ languages already translate the filter form).
    pub fn label_key(self) -> &'static str {
        match self {
            Field::Roots => "folders.edit",
            Field::Name => "filters.name",
            Field::Regex => "filters.regex",
            Field::Fuzzy => "filters.fuzzy",
            Field::Content => "filters.content",
            Field::Glob => "filters.glob",
            Field::Or => "filters.or",
            Field::Not => "filters.not",
            Field::MinSize => "filters.min",
            Field::MaxSize => "filters.max",
            Field::After => "filters.after",
            Field::Before => "filters.before",
            Field::Exclude => "filters.exclude",
            Field::Ext => "filters.ext",
            Field::DbPath => "toolbar.db",
        }
    }
}

/// Keyboard focus: one filter field (with a cursor) or the results.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Field(Field),
    Results,
}

/// Modal popup over the main view.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Popup {
    Help,
    ConfirmDelete,
    Export,
    MoveDest,
    SaveName,
    LoadSaved,
    Language,
    /// Linux/macOS file details (Windows opens the native dialog).
    PropsDetails,
}

/// Messages from the background search thread.
enum WorkerMsg {
    Progress(u64),
    Done {
        run: u64,
        entries: Vec<FileEntry>,
        scanned: u64,
        skipped: u64,
        errors: u64,
        note: String,
    },
}

/// One open search tab: its own folders, filters, results, selection,
/// and sort (mirrors the desktop `SearchTab`).
pub struct Tab {
    pub roots: String,
    pub use_indexed: bool,
    pub db_path: String,
    pub name: String,
    pub regex: String,
    pub fuzzy: String,
    pub content: String,
    pub glob: String,
    pub or_terms: String,
    pub not_terms: String,
    pub min_size: String,
    pub max_size: String,
    pub modified_after: String,
    pub modified_before: String,
    pub exclusions: String,
    pub extensions: String,
    pub case_insensitive: bool,
    pub show_hidden: bool,
    pub results: Vec<FileEntry>,
    pub selected: Option<usize>,
    pub checked: HashSet<PathBuf>,
    pub sort_key: SortKey,
    pub sort_asc: bool,
    pub page: usize,
    pub status: String,
    pub scanned: u64,
    pub running: bool,
    pub searched: bool,
    run_seq: u64,
    worker_rx: Option<Receiver<WorkerMsg>>,
}

impl Tab {
    fn new() -> Self {
        Self {
            roots: String::new(),
            use_indexed: false,
            db_path: String::new(),
            name: String::new(),
            regex: String::new(),
            fuzzy: String::new(),
            content: String::new(),
            glob: String::new(),
            or_terms: String::new(),
            not_terms: String::new(),
            min_size: String::new(),
            max_size: String::new(),
            modified_after: String::new(),
            modified_before: String::new(),
            exclusions: String::new(),
            extensions: String::new(),
            case_insensitive: false,
            show_hidden: false,
            results: Vec::new(),
            selected: None,
            checked: HashSet::new(),
            sort_key: SortKey::Path,
            sort_asc: true,
            page: 0,
            status: String::new(),
            scanned: 0,
            running: false,
            searched: false,
            run_seq: 0,
            worker_rx: None,
        }
    }

    pub fn page_count(&self) -> usize {
        self.results.len().div_ceil(PAGE_SIZE).max(1)
    }

    fn clamp_page(&mut self) {
        let pages = self.page_count();
        if self.page + 1 > pages {
            self.page = pages - 1;
        }
    }

    /// Short tab caption: filter summary plus roots.
    pub fn caption(&self) -> String {
        let what = [
            self.name.trim(),
            self.regex.trim(),
            self.fuzzy.trim(),
            self.content.trim(),
            self.glob.trim(),
        ]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or("*");
        format!("{} @ {}", what, self.roots.trim())
    }
}

/// Split a `;`-separated folder field into roots (same rule as the
/// desktop app: `;` is near-never in real paths, commas stay for
/// extensions/exclusions).
pub fn parse_roots(s: &str) -> Vec<PathBuf> {
    s.split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Split a comma-separated list field (extensions, exclusions,
/// OR/NOT terms).
pub fn parse_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

fn opt_text(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Parse a size field: plain bytes or KB/MB/GB/TB suffix
/// (case-insensitive, optional space, decimals allowed).
pub fn parse_size(s: &str) -> Result<Option<u64>, String> {
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
        return Err(format!("invalid size {s:?}: expected like 1048576 or 10MB"));
    }
    let value: f64 = num
        .parse()
        .map_err(|_| format!("invalid size {s:?}: expected like 1048576 or 10MB"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("invalid size {s:?}: expected like 1048576 or 10MB"));
    }
    Ok(Some((value * mult as f64).round() as u64))
}

fn opt_mtime(s: &str) -> Result<Option<i64>, String> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    parse_mtime(t)
        .map(Some)
        .map_err(|e| format!("invalid date {t:?}: {e}"))
}

/// Sort hits by column and direction (same order as the desktop app).
pub fn sort_entries(entries: &mut [FileEntry], key: SortKey, asc: bool) {
    match key {
        SortKey::Path => entries.sort_by(|a, b| a.path.cmp(&b.path)),
        SortKey::Size => entries.sort_by_key(|e| e.size),
        SortKey::Modified => entries.sort_by_key(|e| e.mtime),
    }
    if !asc {
        entries.reverse();
    }
}

/// Human file size: `0 B`, `1.4 KB`, `23.0 MB`.
pub fn human_size(bytes: u64) -> String {
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

/// Gregorian `(year, month, day)` for whole `days` since the Unix epoch.
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

/// Open a path with the OS default handler.
pub fn open_path(path: &Path) -> Result<(), String> {
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

/// Reveal a path in the OS file manager (select it when supported).
pub fn reveal_path(path: &Path) -> Result<(), String> {
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

#[cfg(windows)]
pub fn show_file_properties(path: &Path) -> Result<(), String> {
    supersearch_core::shell::show_file_properties(path)
}

/// Compact `YYYY-MM-DD` stamp for the results table.
pub fn mtime_short(ts: i64) -> String {
    let (y, m, d) = ymd_from_days(ts.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Full stamp plus relative age for the preview/details pane.
pub fn mtime_display(ts: i64) -> String {
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

/// Export formats offered by the export popup, in order.
pub const EXPORT_FORMATS: [ExportFormat; 5] = [
    ExportFormat::Csv(','),
    ExportFormat::Txt,
    ExportFormat::Html,
    ExportFormat::ExcelXml,
    ExportFormat::Rtf,
];

/// Whole-application state: tabs, focus, popups, config, worker.
pub struct App {
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub focus: Focus,
    /// Cursor byte offset inside the focused field.
    pub cursor: usize,
    pub popup: Option<Popup>,
    /// Selection index inside list popups (load, language, actions).
    pub popup_index: usize,
    /// Export popup: format choice + destination path.
    pub export_format: usize,
    pub export_path: String,
    /// Move popup: destination folder.
    pub move_dest: String,
    /// Save popup: search name.
    pub save_name: String,
    pub preview_visible: bool,
    pub group_cycle: usize,
    pub config: AppConfig,
    pub config_path: PathBuf,
    pub lang: Lang,
    pub tr: I18n,
    pub drives: Vec<PathBuf>,
}

impl App {
    pub fn new(mut config: AppConfig, config_path: PathBuf, initial_root: Option<PathBuf>) -> Self {
        let lang = Lang::from_code(&config.language);
        let mut tab = Tab::new();
        if let Some(root) = initial_root {
            tab.roots = root.display().to_string();
        } else if !config.roots.is_empty() {
            tab.roots = config
                .roots
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("; ");
        }
        if let Some(db) = &config.db_path {
            tab.db_path = db.display().to_string();
        }
        config.language = lang.code().to_string();
        let drives = list_drives();
        Self {
            tabs: vec![tab],
            active: 0,
            focus: Focus::Results,
            cursor: 0,
            popup: None,
            popup_index: 0,
            export_format: 0,
            export_path: String::new(),
            move_dest: String::new(),
            save_name: String::new(),
            preview_visible: true,
            group_cycle: 0,
            config,
            config_path,
            lang,
            tr: I18n::load(lang),
            drives,
        }
    }

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    /// Status assignment that plays borrowck: the translated string is
    /// built before the tab is mutably borrowed.
    fn set_status_t(&mut self, key: &str) {
        let s = self.tr.t(key);
        self.tab_mut().status = s;
    }

    /// [`App::set_status_t`] with `{placeholder}` substitution.
    fn set_status_f(&mut self, key: &str, args: &[(&str, &str)]) {
        let s = self.tr.f(key, args);
        self.tab_mut().status = s;
    }

    fn field_text(&self, field: Field) -> &str {
        let t = self.tab();
        match field {
            Field::Roots => &t.roots,
            Field::Name => &t.name,
            Field::Regex => &t.regex,
            Field::Fuzzy => &t.fuzzy,
            Field::Content => &t.content,
            Field::Glob => &t.glob,
            Field::Or => &t.or_terms,
            Field::Not => &t.not_terms,
            Field::MinSize => &t.min_size,
            Field::MaxSize => &t.max_size,
            Field::After => &t.modified_after,
            Field::Before => &t.modified_before,
            Field::Exclude => &t.exclusions,
            Field::Ext => &t.extensions,
            Field::DbPath => &t.db_path,
        }
    }

    fn field_text_mut(&mut self, field: Field) -> &mut String {
        let t = self.tab_mut();
        match field {
            Field::Roots => &mut t.roots,
            Field::Name => &mut t.name,
            Field::Regex => &mut t.regex,
            Field::Fuzzy => &mut t.fuzzy,
            Field::Content => &mut t.content,
            Field::Glob => &mut t.glob,
            Field::Or => &mut t.or_terms,
            Field::Not => &mut t.not_terms,
            Field::MinSize => &mut t.min_size,
            Field::MaxSize => &mut t.max_size,
            Field::After => &mut t.modified_after,
            Field::Before => &mut t.modified_before,
            Field::Exclude => &mut t.exclusions,
            Field::Ext => &mut t.extensions,
            Field::DbPath => &mut t.db_path,
        }
    }

    /// Focus the field, parking the cursor at the end of its text.
    pub fn focus_field(&mut self, field: Field) {
        self.cursor = self.field_text(field).len();
        self.focus = Focus::Field(field);
    }

    pub fn focus_next(&mut self) {
        self.focus = match self.focus {
            Focus::Field(f) => {
                let i = Field::ALL.iter().position(|&x| x == f).unwrap_or(0);
                if i + 1 < Field::ALL.len() {
                    let next = Field::ALL[i + 1];
                    self.cursor = self.field_text(next).len();
                    Focus::Field(next)
                } else {
                    Focus::Results
                }
            }
            Focus::Results => {
                let first = Field::ALL[0];
                self.cursor = self.field_text(first).len();
                Focus::Field(first)
            }
        };
    }

    pub fn focus_prev(&mut self) {
        self.focus = match self.focus {
            Focus::Field(f) => {
                let i = Field::ALL.iter().position(|&x| x == f).unwrap_or(0);
                if i > 0 {
                    let prev = Field::ALL[i - 1];
                    self.cursor = self.field_text(prev).len();
                    Focus::Field(prev)
                } else {
                    Focus::Results
                }
            }
            Focus::Results => {
                let last = Field::ALL[Field::ALL.len() - 1];
                self.cursor = self.field_text(last).len();
                Focus::Field(last)
            }
        };
    }

    pub fn new_tab(&mut self) {
        let mut tab = Tab::new();
        // New tabs inherit the active tab's folders, like the desktop.
        tab.roots = self.tab().roots.clone();
        tab.db_path = self.tab().db_path.clone();
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.focus_field(Field::Name);
    }

    pub fn close_tab(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.tabs.remove(self.active);
        self.active = self.active.min(self.tabs.len() - 1);
        self.sync_cursor();
    }

    pub fn switch_tab(&mut self, delta: isize) {
        let n = self.tabs.len() as isize;
        self.active = (self.active as isize + delta).rem_euclid(n) as usize;
        self.sync_cursor();
    }

    fn sync_cursor(&mut self) {
        if let Focus::Field(field) = self.focus {
            self.cursor = self.field_text(field).len();
        }
    }

    /// Build the search query from the active tab (same rules as the
    /// desktop app: invalid regex errors, sizes/dates parsed, empty
    /// OR/NOT terms become substring subqueries).
    pub fn build_query(&self) -> Result<Query, String> {
        let t = self.tab();
        if !t.regex.trim().is_empty() && !regex_is_valid(&t.regex) {
            return Err(format!(
                "invalid regex {:?}: matches nothing",
                t.regex.trim()
            ));
        }
        let ors = parse_list(&t.or_terms)
            .into_iter()
            .map(|term| Query {
                name_substr: Some(term),
                ..Query::default()
            })
            .collect();
        let nots = parse_list(&t.not_terms)
            .into_iter()
            .map(|term| Query {
                name_substr: Some(term),
                ..Query::default()
            })
            .collect();
        Ok(Query {
            name_substr: opt_text(&t.name),
            regex: opt_text(&t.regex),
            fuzzy: opt_text(&t.fuzzy),
            content: opt_text(&t.content),
            glob: opt_text(&t.glob),
            extensions: parse_list(&t.extensions),
            case_insensitive: t.case_insensitive,
            min_size: parse_size(&t.min_size)?,
            max_size: parse_size(&t.max_size)?,
            modified_after: opt_mtime(&t.modified_after)?,
            modified_before: opt_mtime(&t.modified_before)?,
            exclude: {
                let mut all = parse_list(&t.exclusions);
                all.extend(self.config.exclusions.iter().cloned());
                all
            },
            or: ors,
            not: nots,
            ..Query::default()
        })
    }

    /// True when at least one filter field is non-empty.
    fn has_any_filter(&self) -> bool {
        let t = self.tab();
        [
            &t.name,
            &t.regex,
            &t.fuzzy,
            &t.content,
            &t.glob,
            &t.or_terms,
            &t.not_terms,
            &t.extensions,
            &t.min_size,
            &t.max_size,
            &t.modified_after,
            &t.modified_before,
            &t.exclusions,
        ]
        .iter()
        .any(|s| !s.trim().is_empty())
    }

    /// Start a search on a worker thread (live roots or the index DB).
    /// Empty searches never run: without a filter the query would
    /// match everything.
    pub fn run_search(&mut self) {
        if !self.has_any_filter() {
            self.set_status_t("status.need_filter");
            return;
        }
        let query = match self.build_query() {
            Ok(q) => q,
            Err(e) => {
                self.tab_mut().status = e;
                return;
            }
        };
        let t = self.tab();
        let use_indexed = t.use_indexed;
        let db_path = PathBuf::from(t.db_path.trim());
        let roots: Vec<PathBuf> = parse_roots(&t.roots);
        if use_indexed && db_path.as_os_str().is_empty() {
            self.set_status_t("status.enter_db");
            return;
        }
        if !use_indexed && roots.iter().all(|r| r.as_os_str().is_empty()) {
            self.set_status_t("status.pick_folder");
            return;
        }
        let (tx, rx) = mpsc::channel::<WorkerMsg>();
        let tab = self.tab_mut();
        tab.run_seq += 1;
        let run = tab.run_seq;
        tab.worker_rx = Some(rx);
        tab.running = true;
        tab.scanned = 0;
        self.set_status_t("status.scanning");
        let (sort_key, sort_asc) = (self.tab().sort_key, self.tab().sort_asc);
        let show_hidden = self.tab().show_hidden;
        let txp = tx.clone();
        std::thread::spawn(move || {
            let progress_cb = move |n: u64| {
                let _ = txp.send(WorkerMsg::Progress(n));
            };
            if use_indexed {
                match load_db(&db_path) {
                    Ok(db_entries) => {
                        let mut entries: Vec<FileEntry> =
                            search(&db_entries, &query).into_iter().cloned().collect();
                        if !show_hidden {
                            entries.retain(|e| {
                                std::fs::metadata(&e.path)
                                    .map(|md| supersearch_index::entry_is_hidden(&e.path, &md))
                                    .map(|hidden| !hidden)
                                    .unwrap_or(true)
                            });
                        }
                        sort_entries(&mut entries, sort_key, sort_asc);
                        let scanned = db_entries.len() as u64;
                        let _ = tx.send(WorkerMsg::Done {
                            run,
                            entries,
                            scanned,
                            skipped: 0,
                            errors: 0,
                            note: format!("index {}", db_path.display()),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(WorkerMsg::Done {
                            run,
                            entries: Vec::new(),
                            scanned: 0,
                            skipped: 0,
                            errors: 1,
                            note: format!("read db {}: {e}", db_path.display()),
                        });
                    }
                }
                return;
            }
            // Missing roots warn instead of failing (same as the CLI).
            let (roots, missing): (Vec<PathBuf>, Vec<PathBuf>) =
                roots.into_iter().partition(|r| r.is_dir());
            if roots.is_empty() {
                let names: Vec<String> = missing.iter().map(|p| p.display().to_string()).collect();
                let _ = tx.send(WorkerMsg::Done {
                    run,
                    entries: Vec::new(),
                    scanned: 0,
                    skipped: 0,
                    errors: 0,
                    note: format!("none of the folders exist: {}", names.join(", ")),
                });
                return;
            }
            let options = ScanOptions {
                roots,
                follow_symlinks: false,
                skip_hidden: !show_hidden,
            };
            // The table shows size/mtime, so metadata is always wanted
            // (same as the desktop app).
            let outcome =
                supersearch_index::fast_search(&options, &query, true, Some(&progress_cb));
            let _ = tx.send(WorkerMsg::Done {
                run,
                scanned: outcome.scanned,
                skipped: outcome.skipped,
                errors: outcome.errors,
                note: outcome.note,
                entries: outcome.entries,
            });
        });
    }

    /// Route each worker back to its originating tab, even when inactive.
    pub fn poll_worker(&mut self) {
        for tab in &mut self.tabs {
            let mut done = None;
            if let Some(rx) = &tab.worker_rx {
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        WorkerMsg::Progress(n) if tab.running => tab.scanned = n,
                        WorkerMsg::Done { .. } => done = Some(msg),
                        _ => {}
                    }
                }
            }
            let Some(WorkerMsg::Done {
                run,
                mut entries,
                scanned,
                skipped,
                errors,
                note,
            }) = done
            else {
                continue;
            };
            if run != tab.run_seq {
                continue;
            }
            tab.worker_rx = None;
            sort_entries(&mut entries, tab.sort_key, tab.sort_asc);
            let present: HashSet<_> = entries.iter().map(|e| e.path.clone()).collect();
            tab.checked.retain(|p| present.contains(p));
            tab.running = false;
            tab.searched = true;
            tab.results = entries;
            tab.page = 0;
            tab.selected = None;
            tab.scanned = scanned;
            tab.status = format!(
                "{} hits ({} scanned via {})",
                tab.results.len(),
                scanned,
                note
            );
            if skipped > 0 || errors > 0 {
                tab.status
                    .push_str(&format!(" ({skipped} skipped, {errors} errors)"));
            }
        }
    }

    /// Cancel a running scan: the worker thread finishes on its own
    /// but its late result is dropped (same as the desktop app).
    pub fn cancel_search(&mut self) {
        let tab = self.tab_mut();
        tab.run_seq += 1;
        tab.worker_rx = None;
        tab.running = false;
        self.set_status_t("status.cancelled");
    }

    /// Move selection inside the current page (`delta` rows).
    pub fn move_selection(&mut self, delta: isize) {
        let tab = self.tab_mut();
        if tab.results.is_empty() {
            return;
        }
        let start = tab.page * PAGE_SIZE;
        let end = (start + PAGE_SIZE).min(tab.results.len());
        if start >= end {
            return;
        }
        tab.selected = Some(match tab.selected {
            Some(i) if i >= start && i < end => {
                (i as isize + delta).clamp(start as isize, end as isize - 1) as usize
            }
            _ => {
                if delta >= 0 {
                    start
                } else {
                    end - 1
                }
            }
        });
    }

    pub fn change_page(&mut self, delta: isize) {
        let tab = self.tab_mut();
        let pages = tab.page_count() as isize;
        tab.page = (tab.page as isize + delta).clamp(0, pages - 1) as usize;
        tab.selected = None;
    }

    /// Switch the sort column (repeat = toggle direction), re-sort.
    pub fn cycle_sort(&mut self) {
        let tab = self.tab_mut();
        tab.sort_key = match tab.sort_key {
            SortKey::Path => SortKey::Size,
            SortKey::Size => SortKey::Modified,
            SortKey::Modified => SortKey::Path,
        };
        tab.sort_asc = true;
        tab.page = 0;
        tab.selected = None;
        let (key, asc) = (tab.sort_key, tab.sort_asc);
        sort_entries(&mut tab.results, key, asc);
    }

    pub fn toggle_sort_dir(&mut self) {
        let tab = self.tab_mut();
        tab.sort_asc = !tab.sort_asc;
        let (key, asc) = (tab.sort_key, tab.sort_asc);
        sort_entries(&mut tab.results, key, asc);
    }

    pub fn toggle_check_selected(&mut self) {
        let tab = self.tab_mut();
        if let Some(i) = tab
            .selected
            .and_then(|i| tab.results.get(i).map(|e| e.path.clone()))
        {
            if !tab.checked.remove(&i) {
                tab.checked.insert(i);
            }
        }
    }

    pub fn check_all(&mut self, check: bool) {
        let tab = self.tab_mut();
        if check {
            tab.checked = tab.results.iter().map(|e| e.path.clone()).collect();
        } else {
            tab.checked.clear();
        }
    }

    fn selected_entry(&self) -> Option<FileEntry> {
        self.tab()
            .selected
            .and_then(|i| self.tab().results.get(i).cloned())
    }

    pub fn open_selected(&mut self) {
        let Some(entry) = self.selected_entry() else {
            self.set_status_t("status.select_row");
            return;
        };
        match open_path(&entry.path) {
            Ok(()) => {
                self.tab_mut().status = self.tr.f(
                    "status.opened",
                    &[("path", &entry.path.display().to_string())],
                );
            }
            Err(e) => self.tab_mut().status = e,
        }
    }

    pub fn reveal_selected(&mut self) {
        let Some(entry) = self.selected_entry() else {
            self.set_status_t("status.select_row");
            return;
        };
        match reveal_path(&entry.path) {
            Ok(()) => {
                self.tab_mut().status = self.tr.f(
                    "status.revealed",
                    &[("path", &entry.path.display().to_string())],
                );
            }
            Err(e) => self.tab_mut().status = e,
        }
    }

    pub fn props_selected(&mut self) {
        let Some(entry) = self.selected_entry() else {
            self.set_status_t("status.select_row");
            return;
        };
        // Native dialog first; if the shell cannot show one,
        // fall back to the in-app details popup so
        // Properties always answers with something useful.
        #[cfg(windows)]
        match show_file_properties(&entry.path) {
            Ok(()) => {}
            Err(e) => {
                self.set_status_f("tui.props_native_failed", &[("e", &e)]);
                self.popup = Some(Popup::PropsDetails);
            }
        }
        #[cfg(not(windows))]
        {
            let _ = entry;
            self.popup = Some(Popup::PropsDetails);
        }
    }

    /// Copy checked paths (all results when nothing is checked) to the
    /// clipboard, newline-joined.
    pub fn yank_checked(&mut self) {
        let paths: Vec<PathBuf> = {
            let tab = self.tab();
            if tab.checked.is_empty() {
                tab.results.iter().map(|e| e.path.clone()).collect()
            } else {
                tab.checked.iter().cloned().collect()
            }
        };
        if paths.is_empty() {
            self.set_status_t("status.nothing_checked");
            return;
        }
        let text = paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let n = paths.len();
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
            Ok(()) => {
                self.set_status_f("status.copied", &[("n", &n.to_string())]);
            }
            Err(e) => {
                self.set_status_f("status.clipboard_fail", &[("e", &e.to_string())]);
            }
        }
    }

    /// Permanently delete every checked file/folder (confirm popup
    /// first: there is no recycle-bin round trip).
    pub fn delete_checked(&mut self) {
        let paths: Vec<PathBuf> = self.tab().checked.iter().cloned().collect();
        if paths.is_empty() {
            self.set_status_t("status.nothing_checked");
            return;
        }
        let mut deleted = Vec::new();
        let mut errors = 0usize;
        for path in &paths {
            let r = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
            if r.is_ok() {
                deleted.push(path.clone());
            } else {
                errors += 1;
            }
        }
        {
            let tab = self.tab_mut();
            tab.results
                .retain(|e| !deleted.iter().any(|p| e.path.starts_with(p)));
            tab.checked
                .retain(|p| !deleted.iter().any(|d| p.starts_with(d)));
            tab.selected = None;
            tab.clamp_page();
            let deleted = deleted.len();
            tab.status = if errors == 0 {
                format!("Deleted {deleted}.")
            } else {
                format!("Deleted {deleted} ({errors} errors).")
            };
        }
        self.popup = None;
    }

    /// Move every checked entry into `dest` (existing names are
    /// skipped, like the desktop app).
    pub fn move_checked(&mut self, dest: &Path) {
        let paths: Vec<PathBuf> = self.tab().checked.iter().cloned().collect();
        if paths.is_empty() {
            self.set_status_t("status.nothing_checked");
            return;
        }
        if !dest.is_dir() {
            self.tab_mut().status = "Choose an existing destination folder.".into();
            return;
        }
        let mut moved = 0usize;
        let mut skipped = 0usize;
        for path in &paths {
            let Some(name) = path.file_name() else {
                skipped += 1;
                continue;
            };
            let target = dest.join(name);
            if target.exists() || std::fs::rename(path, &target).is_err() {
                skipped += 1;
            } else {
                moved += 1;
                let tab = self.tab_mut();
                for slot in &mut tab.results {
                    if let Ok(relative) = slot.path.strip_prefix(path) {
                        slot.path = target.join(relative);
                    }
                }
                tab.checked.retain(|p| !p.starts_with(path));
            }
        }
        let tab = self.tab_mut();
        sort_entries(&mut tab.results, tab.sort_key, tab.sort_asc);
        tab.selected = None;
        tab.status = format!("Moved {moved} ({skipped} skipped).");
        self.popup = None;
    }

    /// Export checked rows (all rows when nothing is checked) to a file.
    pub fn export_now(&mut self) {
        let format = EXPORT_FORMATS[self.export_format.min(EXPORT_FORMATS.len() - 1)];
        let dest = PathBuf::from(self.export_path.trim());
        if dest.as_os_str().is_empty() {
            self.set_status_t("tui.export_no_path");
            return;
        }
        let entries: Vec<FileEntry> = {
            let tab = self.tab();
            if tab.checked.is_empty() {
                tab.results.clone()
            } else {
                tab.results
                    .iter()
                    .filter(|e| tab.checked.contains(&e.path))
                    .cloned()
                    .collect()
            }
        };
        let label_key = format!(
            "export.{}",
            match format {
                ExportFormat::Csv(_) => "csv",
                ExportFormat::Txt => "txt",
                ExportFormat::Html => "html",
                ExportFormat::ExcelXml => "xml",
                ExportFormat::Rtf => "rtf",
            }
        );
        let label = self.tr.t(&label_key);
        let total = entries.len();
        match std::fs::File::create(&dest)
            .map_err(|e| e.to_string())
            .and_then(|mut out| {
                export::write_format(&mut out, &entries, format, None).map_err(|e| e.to_string())
            }) {
            Ok(()) => {
                self.tab_mut().status = self.tr.f(
                    "status.exported",
                    &[("total", &total.to_string()), ("label", &label)],
                );
            }
            Err(e) => {
                self.set_status_f("status.export_failed", &[("e", &e)]);
            }
        }
        self.popup = None;
    }

    /// Save the active tab as a named search (same snapshot shape as
    /// the desktop app, so both share the config file).
    pub fn save_current_search(&mut self) {
        let name = self.save_name.trim().to_string();
        if name.is_empty() {
            self.set_status_t("tui.save_no_name");
            return;
        }
        let t = self.tab();
        let saved = SavedSearch {
            name,
            name_filter: opt_text(&t.name),
            regex: opt_text(&t.regex),
            fuzzy: opt_text(&t.fuzzy),
            content: opt_text(&t.content),
            min_size: parse_size(&t.min_size).unwrap_or(None),
            max_size: parse_size(&t.max_size).unwrap_or(None),
            modified_after: opt_mtime(&t.modified_after).unwrap_or(None),
            modified_before: opt_mtime(&t.modified_before).unwrap_or(None),
            glob: opt_text(&t.glob),
            extensions: parse_list(&t.extensions),
            case_insensitive: t.case_insensitive,
            exclude: parse_list(&t.exclusions),
            root: t.roots.clone(),
            or_terms: parse_list(&t.or_terms),
            not_terms: parse_list(&t.not_terms),
            show_hidden: t.show_hidden,
        };
        let label = saved.name.clone();
        self.config.add_saved_search(saved);
        self.persist_config();
        self.set_status_f("status.saved_saved", &[("name", &label)]);
        self.popup = None;
    }

    /// Apply a saved search to the active tab.
    pub fn load_saved_search(&mut self, index: usize) {
        let Some(saved) = self.config.saved_searches.get(index).cloned() else {
            return;
        };
        let label = saved.name.clone();
        if self.tab().running {
            self.cancel_search();
        }
        let t = self.tab_mut();
        t.name = saved.name_filter.unwrap_or_default();
        t.regex = saved.regex.unwrap_or_default();
        t.fuzzy = saved.fuzzy.unwrap_or_default();
        t.content = saved.content.unwrap_or_default();
        t.glob = saved.glob.unwrap_or_default();
        t.min_size = saved.min_size.map(|v| v.to_string()).unwrap_or_default();
        t.max_size = saved.max_size.map(|v| v.to_string()).unwrap_or_default();
        t.modified_after = saved
            .modified_after
            .map(|v| v.to_string())
            .unwrap_or_default();
        t.modified_before = saved
            .modified_before
            .map(|v| v.to_string())
            .unwrap_or_default();
        t.extensions = saved.extensions.join(", ");
        t.case_insensitive = saved.case_insensitive;
        t.exclusions = saved.exclude.join(", ");
        t.roots = saved.root;
        t.or_terms = saved.or_terms.join(", ");
        t.not_terms = saved.not_terms.join(", ");
        t.show_hidden = saved.show_hidden;
        t.results.clear();
        t.checked.clear();
        t.selected = None;
        t.page = 0;
        t.searched = false;
        let status = self.tr.f("status.saved_loaded", &[("name", &label)]);
        self.tab_mut().status = status;
        self.popup = None;
        self.sync_cursor();
    }

    pub fn set_language(&mut self, lang: Lang) {
        self.lang = lang;
        self.tr = I18n::load(lang);
        self.config.language = lang.code().to_string();
        self.persist_config();
        self.popup = None;
    }

    /// Cycle the extension field through the built-in file groups
    /// (images, audio, video, archives, code, documents), then clear.
    pub fn cycle_group(&mut self) {
        let groups = builtin_file_groups();
        if self.group_cycle < groups.len() {
            let g = &groups[self.group_cycle];
            self.tab_mut().extensions = g.extensions.join(", ");
            self.group_cycle += 1;
        } else {
            self.tab_mut().extensions.clear();
            self.group_cycle = 0;
        }
    }

    pub fn persist_config(&self) {
        let _ = self.config.save_to(&self.config_path);
    }

    /// Preview text for the selected entry: first lines of a file,
    /// a child count for a folder, nothing without a selection.
    pub fn preview_text(&self) -> String {
        let Some(entry) = self.selected_entry() else {
            return self.tr.t("tui.preview_empty").to_string();
        };
        if entry.is_dir {
            let count = std::fs::read_dir(&entry.path)
                .map(|r| r.count())
                .unwrap_or(0);
            return format!("{} ({count})", entry.path.display());
        }
        let Ok(bytes) = read_prefix(&entry.path, PREVIEW_BYTES) else {
            return format!("(cannot open file: {})", entry.path.display());
        };
        let text = supersearch_query::decode_text_bytes(&bytes);
        text.lines()
            .take(PREVIEW_LINES)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Quick-select drives: every present `X:\` on Windows; `/` plus the
/// existing top-level mounts on Unix (same as the desktop app).
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
            if dir.is_dir() && !out.iter().any(|p| p == &dir) {
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

fn read_prefix(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    f.by_ref().take(max).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Edit a single-line string at a char-boundary cursor.
fn edit_line(text: &mut String, cursor: &mut usize, key: &KeyEvent) {
    *cursor = (*cursor).min(text.len());
    while !text.is_char_boundary(*cursor) {
        *cursor -= 1;
    }
    match key.code {
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            text.insert(*cursor, c);
            *cursor += c.len_utf8();
        }
        KeyCode::Backspace => {
            if *cursor > 0 {
                let prev = text[..*cursor]
                    .chars()
                    .next_back()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                *cursor -= prev;
                text.remove(*cursor);
            }
        }
        KeyCode::Delete => {
            if *cursor < text.len() && text.is_char_boundary(*cursor) {
                text.remove(*cursor);
            }
        }
        KeyCode::Left => {
            if *cursor > 0 {
                let prev = text[..*cursor]
                    .chars()
                    .next_back()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                *cursor -= prev;
            }
        }
        KeyCode::Right => {
            if *cursor < text.len() {
                let next = text[*cursor..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                *cursor += next;
            }
        }
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = text.len(),
        _ => {}
    }
}

impl App {
    /// Handle one key event. Returns true when the app should quit.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('q') => return true,
                KeyCode::Char('r') => {
                    self.popup = None;
                    self.run_search();
                    return false;
                }
                KeyCode::Char('t') => {
                    self.popup = None;
                    self.new_tab();
                    return false;
                }
                KeyCode::Char('w') => {
                    self.popup = None;
                    self.close_tab();
                    return false;
                }
                KeyCode::Char('s') => {
                    self.open_save_popup();
                    return false;
                }
                // Ctrl+O / Ctrl+L mirror F3/F4: some terminals and
                // multiplexers swallow function keys.
                KeyCode::Char('o') => {
                    self.popup_index = 0;
                    self.popup = Some(Popup::LoadSaved);
                    return false;
                }
                KeyCode::Char('l') => {
                    self.popup_index = 0;
                    self.popup = Some(Popup::Language);
                    return false;
                }
                _ => {}
            }
        }
        if self.popup.is_some() {
            return self.on_key_popup(key);
        }
        match self.focus {
            Focus::Field(f) => self.on_key_field(key, f),
            Focus::Results => self.on_key_results(key),
        }
    }

    fn on_key_popup(&mut self, key: KeyEvent) -> bool {
        match self.popup.clone() {
            Some(Popup::Help) | Some(Popup::PropsDetails) => {
                self.popup = None;
                false
            }
            Some(Popup::ConfirmDelete) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.delete_checked();
                    false
                }
                _ => {
                    self.popup = None;
                    false
                }
            },
            Some(Popup::Export) => match key.code {
                KeyCode::Esc => {
                    self.popup = None;
                    false
                }
                KeyCode::Enter => {
                    self.export_now();
                    false
                }
                KeyCode::Up => {
                    self.export_format =
                        (self.export_format + EXPORT_FORMATS.len() - 1) % EXPORT_FORMATS.len();
                    false
                }
                KeyCode::Down => {
                    self.export_format = (self.export_format + 1) % EXPORT_FORMATS.len();
                    false
                }
                _ => {
                    edit_line(&mut self.export_path, &mut self.cursor, &key);
                    false
                }
            },
            Some(Popup::MoveDest) => match key.code {
                KeyCode::Esc => {
                    self.popup = None;
                    false
                }
                KeyCode::Enter => {
                    let dest = PathBuf::from(self.move_dest.trim());
                    self.move_checked(&dest);
                    false
                }
                _ => {
                    edit_line(&mut self.move_dest, &mut self.cursor, &key);
                    false
                }
            },
            Some(Popup::SaveName) => match key.code {
                KeyCode::Esc => {
                    self.popup = None;
                    false
                }
                KeyCode::Enter => {
                    self.save_current_search();
                    false
                }
                _ => {
                    edit_line(&mut self.save_name, &mut self.cursor, &key);
                    false
                }
            },
            Some(Popup::LoadSaved) => {
                let n = self.config.saved_searches.len().max(1);
                match key.code {
                    KeyCode::Esc => self.popup = None,
                    KeyCode::Up => {
                        self.popup_index = (self.popup_index + n - 1) % n;
                    }
                    KeyCode::Down => {
                        self.popup_index = (self.popup_index + 1) % n;
                    }
                    KeyCode::Enter => {
                        if !self.config.saved_searches.is_empty() {
                            let i = self.popup_index.min(self.config.saved_searches.len() - 1);
                            self.load_saved_search(i);
                        } else {
                            self.popup = None;
                        }
                    }
                    _ => {}
                }
                false
            }
            Some(Popup::Language) => {
                let n = Lang::all().len().max(1);
                match key.code {
                    KeyCode::Esc => self.popup = None,
                    KeyCode::Up => {
                        self.popup_index = (self.popup_index + n - 1) % n;
                    }
                    KeyCode::Down => {
                        self.popup_index = (self.popup_index + 1) % n;
                    }
                    KeyCode::Enter => {
                        let langs = Lang::all();
                        if let Some(lang) = langs.get(self.popup_index.min(n - 1)) {
                            self.set_language(*lang);
                        } else {
                            self.popup = None;
                        }
                    }
                    _ => {}
                }
                false
            }
            None => false,
        }
    }

    fn on_key_field(&mut self, key: KeyEvent, field: Field) -> bool {
        match key.code {
            KeyCode::Tab => {
                self.focus_next();
                false
            }
            KeyCode::BackTab => {
                self.focus_prev();
                false
            }
            KeyCode::Enter => {
                self.focus = Focus::Results;
                self.run_search();
                false
            }
            KeyCode::Esc => {
                self.focus = Focus::Results;
                false
            }
            KeyCode::Up => {
                self.focus_prev();
                if matches!(self.focus, Focus::Results) {
                    self.focus_field(Field::ALL[Field::ALL.len() - 1]);
                }
                false
            }
            KeyCode::Down => {
                self.focus_next();
                false
            }
            KeyCode::F(5) => {
                self.run_search();
                false
            }
            _ => {
                // Scoped borrow dance so the cursor and the field can
                // both be borrowed mutably.
                let mut cursor = self.cursor;
                {
                    let text = self.field_text_mut(field);
                    edit_line(text, &mut cursor, &key);
                }
                self.cursor = cursor;
                false
            }
        }
    }

    fn on_key_results(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Tab => {
                self.focus_next();
                false
            }
            KeyCode::BackTab => {
                self.focus_prev();
                false
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                false
            }
            KeyCode::PageUp => {
                self.change_page(-1);
                false
            }
            KeyCode::PageDown => {
                self.change_page(1);
                false
            }
            KeyCode::Home => {
                let start = self.tab().page * PAGE_SIZE;
                if start < self.tab().results.len() {
                    self.tab_mut().selected = Some(start);
                }
                false
            }
            KeyCode::End => {
                let tab = self.tab();
                let end = ((tab.page + 1) * PAGE_SIZE).min(tab.results.len());
                if end > tab.page * PAGE_SIZE {
                    self.tab_mut().selected = Some(end - 1);
                }
                false
            }
            KeyCode::Char(' ') => {
                self.toggle_check_selected();
                false
            }
            KeyCode::Enter | KeyCode::Char('o') => {
                self.open_selected();
                false
            }
            KeyCode::Char('r') => {
                self.reveal_selected();
                false
            }
            KeyCode::Char('p') => {
                self.props_selected();
                false
            }
            KeyCode::Char('y') => {
                self.yank_checked();
                false
            }
            KeyCode::Char('e') => {
                self.open_export_popup();
                false
            }
            KeyCode::Char('d') => {
                if self.tab().checked.is_empty() {
                    self.set_status_t("status.nothing_checked");
                } else {
                    self.popup = Some(Popup::ConfirmDelete);
                }
                false
            }
            KeyCode::Char('m') => {
                if self.tab().checked.is_empty() {
                    self.set_status_t("status.nothing_checked");
                } else {
                    self.move_dest.clear();
                    self.cursor = 0;
                    self.popup = Some(Popup::MoveDest);
                }
                false
            }
            KeyCode::Char('v') => {
                self.preview_visible = !self.preview_visible;
                false
            }
            KeyCode::Char('s') => {
                self.cycle_sort();
                false
            }
            KeyCode::Char('S') => {
                self.toggle_sort_dir();
                false
            }
            KeyCode::Char('a') => {
                self.check_all(true);
                false
            }
            KeyCode::Char('n') => {
                self.check_all(false);
                false
            }
            KeyCode::Char('i') => {
                let v = !self.tab().case_insensitive;
                self.tab_mut().case_insensitive = v;
                false
            }
            KeyCode::Char('h') => {
                let v = !self.tab().show_hidden;
                self.tab_mut().show_hidden = v;
                false
            }
            KeyCode::Char('I') => {
                let v = !self.tab().use_indexed;
                self.tab_mut().use_indexed = v;
                false
            }
            KeyCode::Char('q') => true,
            KeyCode::Char('?') => {
                self.popup = Some(Popup::Help);
                false
            }
            KeyCode::Char('[') => {
                self.switch_tab(-1);
                false
            }
            KeyCode::Char(']') => {
                self.switch_tab(1);
                false
            }
            KeyCode::Esc => {
                if self.tab().running {
                    self.cancel_search();
                }
                false
            }
            KeyCode::F(1) => {
                self.popup = Some(Popup::Help);
                false
            }
            KeyCode::F(2) => {
                self.open_save_popup();
                false
            }
            KeyCode::F(3) => {
                self.popup_index = 0;
                self.popup = Some(Popup::LoadSaved);
                false
            }
            KeyCode::F(4) => {
                self.popup_index = 0;
                self.popup = Some(Popup::Language);
                false
            }
            KeyCode::F(5) => {
                self.run_search();
                false
            }
            KeyCode::F(6) => {
                self.cycle_group();
                false
            }
            _ => false,
        }
    }

    fn open_save_popup(&mut self) {
        self.save_name.clear();
        self.cursor = 0;
        self.popup = Some(Popup::SaveName);
    }

    fn open_export_popup(&mut self) {
        if self.export_path.trim().is_empty() {
            self.export_path = "supersearch-results.csv".to_string();
        }
        self.cursor = self.export_path.len();
        self.popup = Some(Popup::Export);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(
            AppConfig::default(),
            PathBuf::from("test-config.json"),
            None,
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn roots_split_on_semicolons_not_commas() {
        assert_eq!(
            parse_roots("C:\\; D:\\data "),
            vec![PathBuf::from("C:\\"), PathBuf::from("D:\\data")]
        );
        assert!(parse_roots("").is_empty());
    }

    #[test]
    fn list_fields_split_commas_and_trim() {
        assert_eq!(parse_list("rs, md,, py "), vec!["rs", "md", "py"]);
    }

    #[test]
    fn size_parser_accepts_suffixes_and_rejects_garbage() {
        assert_eq!(parse_size("").unwrap(), None);
        assert_eq!(parse_size("1048576").unwrap(), Some(1_048_576));
        assert_eq!(parse_size("10MB").unwrap(), Some(10 * 1024 * 1024));
        assert_eq!(parse_size("1.5 kb").unwrap(), Some(1536));
        assert!(parse_size("lots").is_err());
        assert!(parse_size("-5").is_err());
    }

    #[test]
    fn empty_search_never_runs() {
        let mut app = test_app();
        app.tab_mut().roots = "C:\\".to_string();
        app.run_search();
        assert!(!app.tab().running);
        assert!(app.tab().worker_rx.is_none());
    }

    #[test]
    fn invalid_regex_is_a_status_error_not_a_run() {
        let mut app = test_app();
        app.tab_mut().roots = "C:\\".to_string();
        app.tab_mut().name = "x".to_string();
        app.tab_mut().regex = "(".to_string();
        app.run_search();
        assert!(!app.tab().running);
        assert!(app.tab().status.contains("regex"));
    }

    #[test]
    fn focus_tabs_through_every_field_then_results() {
        let mut app = test_app();
        app.focus = Focus::Results;
        for (i, &field) in Field::ALL.iter().enumerate() {
            app.focus_next();
            assert_eq!(app.focus, Focus::Field(field), "step {i}");
        }
        app.focus_next();
        assert_eq!(app.focus, Focus::Results);
        app.focus_prev();
        assert_eq!(app.focus, Focus::Field(Field::DbPath));
    }

    #[test]
    fn field_typing_inserts_at_cursor() {
        let mut app = test_app();
        app.focus_field(Field::Name);
        app.on_key(key(KeyCode::Char('a')));
        app.on_key(key(KeyCode::Char('b')));
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Char('X')));
        assert_eq!(app.tab().name, "aXb");
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.tab().name, "ab");
    }

    #[test]
    fn sort_cycles_columns_then_reverses() {
        let mut app = test_app();
        app.tab_mut().results = vec![
            FileEntry::new(PathBuf::from("b"), 2, 2, false),
            FileEntry::new(PathBuf::from("a"), 1, 1, false),
        ];
        app.cycle_sort();
        assert_eq!(app.tab().sort_key, SortKey::Size);
        assert_eq!(app.tab().results[0].path, PathBuf::from("a"));
        app.toggle_sort_dir();
        assert_eq!(app.tab().results[0].path, PathBuf::from("b"));
    }

    #[test]
    fn paging_clamps_and_clears_selection() {
        let mut app = test_app();
        app.tab_mut().results = (0..250)
            .map(|i| FileEntry::new(PathBuf::from(format!("f{i:03}")), i as u64, 0, false))
            .collect();
        app.change_page(1);
        assert_eq!(app.tab().page, 1);
        app.move_selection(1);
        assert_eq!(app.tab().selected, Some(PAGE_SIZE));
        app.move_selection(1);
        assert_eq!(app.tab().selected, Some(PAGE_SIZE + 1));
        app.change_page(5);
        assert_eq!(app.tab().page, 2);
        assert_eq!(app.tab().selected, None);
    }

    #[test]
    fn query_builder_maps_every_filter() {
        let mut app = test_app();
        let t = app.tab_mut();
        t.name = "report".to_string();
        t.extensions = "pdf, md".to_string();
        t.min_size = "1KB".to_string();
        t.case_insensitive = true;
        t.or_terms = "a, b".to_string();
        t.not_terms = "tmp".to_string();
        let q = app.build_query().expect("valid filters");
        assert_eq!(q.name_substr.as_deref(), Some("report"));
        assert_eq!(q.extensions, vec!["pdf", "md"]);
        assert_eq!(q.min_size, Some(1024));
        assert!(q.case_insensitive);
        assert_eq!(q.or.len(), 2);
        assert_eq!(q.not.len(), 1);
    }

    #[test]
    fn full_frame_renders_without_panicking() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = test_app();
        app.tab_mut().name = "report".to_string();
        app.tab_mut().roots = "C:\\".to_string();
        app.tab_mut().results = vec![
            FileEntry::new(PathBuf::from("C:\\a.txt"), 10, 1_700_000_000, false),
            FileEntry::new(PathBuf::from("C:\\b"), 0, 1_700_000_100, true),
        ];
        app.tab_mut().selected = Some(0);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| crate::ui::draw(f, &app))
            .expect("draw works headless");
        let buf = terminal.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("Results (2)"));
        assert!(text.contains("report"));
        // Every popup also renders without panicking.
        for popup in [
            Popup::Help,
            Popup::ConfirmDelete,
            Popup::Export,
            Popup::MoveDest,
            Popup::SaveName,
            Popup::LoadSaved,
            Popup::Language,
            Popup::PropsDetails,
        ] {
            app.popup = Some(popup);
            terminal
                .draw(|f| crate::ui::draw(f, &app))
                .expect("popup draws headless");
        }
    }

    #[test]
    fn cancel_drops_late_worker_results() {
        let mut app = test_app();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        app.tab_mut().roots = dir.path().display().to_string();
        app.tab_mut().name = "a".to_string();
        app.run_search();
        assert!(app.tab().running);
        app.cancel_search();
        assert!(!app.tab().running);
        // Drain until the worker is reaped (bounded: the scan is tiny).
        for _ in 0..100 {
            app.poll_worker();
            std::thread::sleep(std::time::Duration::from_millis(10));
            if app.tab().worker_rx.is_none() {
                break;
            }
        }
        app.poll_worker();
        assert!(app.tab().results.is_empty());
        assert!(app.tab().status.contains("ancell"));
    }

    #[test]
    fn results_q_quits_fields_q_types() {
        let mut app = test_app();
        app.focus = Focus::Results;
        assert!(app.on_key(key(KeyCode::Char('q'))));
        app.focus_field(Field::Name);
        assert!(!app.on_key(key(KeyCode::Char('q'))));
        assert_eq!(app.tab().name, "q");
    }

    #[test]
    fn worker_results_and_progress_stay_with_the_original_tab() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.tab_mut().worker_rx = Some(rx);
        app.tab_mut().running = true;
        app.tab_mut().checked.insert(PathBuf::from("stale"));
        app.new_tab();
        tx.send(WorkerMsg::Progress(9)).unwrap();
        app.poll_worker();
        assert_eq!(app.tabs[0].scanned, 9);
        assert_eq!(app.tab().scanned, 0);
        tx.send(WorkerMsg::Done {
            run: 0,
            entries: vec![FileEntry::new("original.txt".into(), 1, 0, false)],
            scanned: 10,
            skipped: 0,
            errors: 0,
            note: "fixture".into(),
        })
        .unwrap();
        app.poll_worker();
        assert_eq!(app.tabs[0].results[0].path, PathBuf::from("original.txt"));
        assert!(!app.tabs[0].running);
        assert!(app.tabs[0].checked.is_empty());
        assert!(app.tab().results.is_empty());
    }

    #[test]
    fn live_search_finds_fixture_and_sorts_results() {
        // Relative roots deliberately use the directory walker. An absolute
        // temporary folder on an elevated runner can enumerate its entire
        // NTFS volume, making this small worker test depend on disk size.
        let dir = tempfile::tempdir_in(".").unwrap();
        let root = PathBuf::from(".")
            .join(dir.path().file_name().unwrap())
            .join("root");
        assert!(!root.is_absolute());
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), "TODO small").unwrap();
        std::fs::write(root.join("b.txt"), "TODO substantially larger").unwrap();
        std::fs::write(root.join("c.rs"), "TODO wrong extension").unwrap();
        let mut app = test_app();
        let t = app.tab_mut();
        t.roots = root.display().to_string();
        t.content = "TODO".into();
        t.extensions = "txt".into();
        t.sort_key = SortKey::Size;
        t.sort_asc = false;
        app.run_search();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.tab().running && std::time::Instant::now() < deadline {
            app.poll_worker();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!app.tab().running, "fixture search timed out");
        assert_eq!(app.tab().results.len(), 2, "{}", app.tab().status);
        assert_eq!(app.tab().results[0].path, root.join("b.txt"));
    }

    #[test]
    fn unicode_editing_survives_tab_switch_and_invalid_cursor() {
        let mut app = test_app();
        app.tab_mut().name = "a".into();
        app.new_tab();
        app.tab_mut().name = "日本語".into();
        app.focus_field(Field::Name);
        app.switch_tab(-1);
        assert_eq!(app.cursor, 1);
        app.switch_tab(1);
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.tab().name, "日本");
        let mut text = "日本".to_string();
        let mut cursor = 2;
        edit_line(&mut text, &mut cursor, &key(KeyCode::Char('x')));
        assert_eq!(text, "x日本");
    }

    #[test]
    fn failed_deletes_remain_visible_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("already-missing.txt");
        let mut app = test_app();
        app.tab_mut()
            .results
            .push(FileEntry::new(path.clone(), 1, 0, false));
        app.tab_mut().checked.insert(path.clone());
        app.delete_checked();
        assert_eq!(app.tab().results.len(), 1);
        assert!(app.tab().checked.contains(&path));
        assert!(app.tab().status.contains("1 errors"));
    }

    #[test]
    fn delete_popup_requires_explicit_y() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keep.txt");
        std::fs::write(&path, "keep").unwrap();
        let mut app = test_app();
        app.tab_mut().checked.insert(path.clone());
        app.popup = Some(Popup::ConfirmDelete);
        app.on_key(key(KeyCode::Enter));
        assert!(path.exists());
        assert_eq!(app.popup, None);
    }

    #[test]
    fn move_skips_collisions_and_updates_descendant_paths() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let dest = dir.path().join("dest");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&dest).unwrap();
        let child = source.join("child.txt");
        std::fs::write(&child, "child").unwrap();
        let mut app = test_app();
        app.tab_mut().results = vec![
            FileEntry::new(source.clone(), 0, 0, true),
            FileEntry::new(child, 5, 0, false),
        ];
        app.tab_mut().checked.insert(source.clone());
        app.move_checked(&dest);
        assert!(dest.join("source/child.txt").exists());
        assert!(app
            .tab()
            .results
            .iter()
            .all(|e| e.path.starts_with(dest.join("source"))));
        assert!(app.tab().checked.is_empty());
        let collision = dir.path().join("child.txt");
        std::fs::write(&collision, "original").unwrap();
        std::fs::write(dest.join("child.txt"), "existing").unwrap();
        app.tab_mut().checked.insert(collision.clone());
        app.move_checked(&dest);
        assert_eq!(
            std::fs::read_to_string(dest.join("child.txt")).unwrap(),
            "existing"
        );
        assert!(app.tab().checked.contains(&collision));
    }

    #[test]
    fn selection_stays_visible_in_results_and_language_lists() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = test_app();
        app.preview_visible = false;
        app.tab_mut().results = (0..100)
            .map(|i| FileEntry::new(format!("file{i:03}").into(), 1, 0, false))
            .collect();
        app.tab_mut().selected = Some(90);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("file090"));
        let langs = Lang::all();
        app.popup = Some(Popup::Language);
        app.popup_index = langs.len() - 1;
        terminal.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains(&format!(" · {}", langs.last().unwrap().code())));
    }
}

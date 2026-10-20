//! Persistent `supersearch` configuration.
//!
//! [`AppConfig`] is stored as pretty-printed JSON (see
//! [`default_config_path`]) and covers everything the CLI and GUI need to
//! restore across runs: scan roots, the SQLite index path, window state,
//! named saved searches, recent queries, exclusion patterns, and presets.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Maximum number of entries kept in [`AppConfig::recents`].
pub const MAX_RECENTS: usize = 50;

/// Window geometry remembered across GUI runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            width: 1024.0,
            height: 768.0,
            maximized: false,
        }
    }
}

/// A named set of search filters.
///
/// The fields mirror the flat (non-recursive) half of
/// [`supersearch_query::Query`]; use [`SavedSearch::to_query`] to run one.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SavedSearch {
    pub name: String,
    pub name_filter: Option<String>,
    pub regex: Option<String>,
    pub fuzzy: Option<String>,
    pub content: Option<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
    pub glob: Option<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub case_insensitive: bool,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// `;`-separated search roots (kept out of the query: roots are a
    /// scan option, not a filter). Empty means "wherever opened".
    #[serde(default)]
    pub root: String,
    /// OR/NOT terms as plain substrings. `from_query` keeps only
    /// single-substring subqueries here (the only kind the GUI builds);
    /// richer CLI-built subqueries do not round-trip through this.
    #[serde(default)]
    pub or_terms: Vec<String>,
    #[serde(default)]
    pub not_terms: Vec<String>,
    /// Include hidden files and folders.
    #[serde(default)]
    pub show_hidden: bool,
}

impl SavedSearch {
    /// Convert into a runnable query. OR/NOT terms become
    /// single-substring subqueries, mirroring what the GUI builds.
    pub fn to_query(&self) -> supersearch_query::Query {
        let sub = |terms: &[String]| {
            terms
                .iter()
                .map(|t| supersearch_query::Query {
                    name_substr: Some(t.clone()),
                    ..supersearch_query::Query::default()
                })
                .collect()
        };
        supersearch_query::Query {
            name_substr: self.name_filter.clone(),
            regex: self.regex.clone(),
            fuzzy: self.fuzzy.clone(),
            content: self.content.clone(),
            min_size: self.min_size,
            max_size: self.max_size,
            modified_after: self.modified_after,
            modified_before: self.modified_before,
            glob: self.glob.clone(),
            extensions: self.extensions.clone(),
            case_insensitive: self.case_insensitive,
            exclude: self.exclude.clone(),
            or: sub(&self.or_terms),
            not: sub(&self.not_terms),
            ..supersearch_query::Query::default()
        }
    }

    /// Build from a runnable query plus a display name. Only
    /// single-substring OR/NOT subqueries survive (see
    /// [`SavedSearch::or_terms`]); roots and the hidden flag travel
    /// outside the query and must be set by the caller.
    pub fn from_query(name: impl Into<String>, query: &supersearch_query::Query) -> Self {
        let terms = |subs: &[supersearch_query::Query]| {
            subs.iter().filter_map(|q| q.name_substr.clone()).collect()
        };
        Self {
            name: name.into(),
            name_filter: query.name_substr.clone(),
            regex: query.regex.clone(),
            fuzzy: query.fuzzy.clone(),
            content: query.content.clone(),
            min_size: query.min_size,
            max_size: query.max_size,
            modified_after: query.modified_after,
            modified_before: query.modified_before,
            glob: query.glob.clone(),
            extensions: query.extensions.clone(),
            case_insensitive: query.case_insensitive,
            exclude: query.exclude.clone(),
            root: String::new(),
            or_terms: terms(&query.or),
            not_terms: terms(&query.not),
            show_hidden: false,
        }
    }
}

/// One recent search: either a full filter snapshot (`Full`, written by
/// current versions) or a bare name string (`Legacy`, written by older
/// versions that only remembered the name filter). Untagged so existing
/// config files keep loading; new runs only write `Full`, and clicking
/// one restores every filter instead of just the name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RecentEntry {
    Full(SavedSearch),
    Legacy(String),
}

impl RecentEntry {
    /// Short button label: the name filter when there is one, else a
    /// compact summary of the other filters.
    pub fn label(&self) -> String {
        match self {
            RecentEntry::Legacy(s) => s.clone(),
            RecentEntry::Full(saved) => {
                let name = saved.name_filter.as_deref().map(str::trim).unwrap_or("");
                if !name.is_empty() {
                    return name.to_string();
                }
                Self::describe(saved)
            }
        }
    }

    /// Filter summary for translated tooltips: the same compact
    /// summary as [`RecentEntry::details`]'s middle, without the name
    /// and folders (the caller adds those in its own language).
    pub fn filters_summary(&self) -> String {
        match self {
            RecentEntry::Legacy(_) => String::new(),
            RecentEntry::Full(saved) => Self::describe(saved),
        }
    }

    /// Tooltip text: display name plus the filter summary and the
    /// stored folders (legacy bare strings only ever held a name).
    pub fn details(&self) -> String {
        match self {
            RecentEntry::Legacy(s) => format!("{s} (name filter only)"),
            RecentEntry::Full(saved) => {
                let where_ = if saved.root.trim().is_empty() {
                    "current tab's folders".to_string()
                } else {
                    saved.root.clone()
                };
                format!(
                    "{} — {} — folders: {where_}",
                    saved.name,
                    Self::describe(saved)
                )
            }
        }
    }

    /// Compact `key:value` summary of a snapshot's set filters, mirroring
    /// the CLI `describe_search` wording.
    fn describe(saved: &SavedSearch) -> String {
        let mut parts = Vec::new();
        if let Some(v) = saved.regex.as_deref() {
            parts.push(format!("regex:{v}"));
        }
        if let Some(v) = saved.fuzzy.as_deref() {
            parts.push(format!("fuzzy:{v}"));
        }
        if let Some(v) = saved.content.as_deref() {
            parts.push(format!("content:{v}"));
        }
        if let Some(v) = saved.glob.as_deref() {
            parts.push(format!("glob:{v}"));
        }
        if !saved.extensions.is_empty() {
            parts.push(format!("ext:{}", saved.extensions.join(",")));
        }
        if !saved.or_terms.is_empty() {
            parts.push(format!("any:{}", saved.or_terms.join(",")));
        }
        if !saved.not_terms.is_empty() {
            parts.push(format!("not:{}", saved.not_terms.join(",")));
        }
        if parts.is_empty() {
            "all files".to_string()
        } else {
            parts.join(" ")
        }
    }
}

/// A named shortcut for a commonly reused filter slice.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub exclusions: Vec<String>,
}

/// A named extension set ("Documents", "Images", …) for `--group` and
/// the GUI group picker. Custom groups in [`AppConfig::file_groups`]
/// override built-ins of the same name (case-insensitive).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FileGroup {
    pub name: String,
    #[serde(default)]
    pub extensions: Vec<String>,
}

/// Built-in file groups for common file types. Customs
/// loaded from the config replace these by name.
pub fn builtin_file_groups() -> Vec<FileGroup> {
    let group = |name: &str, extensions: &[&str]| FileGroup {
        name: name.to_string(),
        extensions: extensions.iter().map(|e| e.to_string()).collect(),
    };
    vec![
        group(
            "Documents",
            &[
                "txt", "md", "doc", "docx", "odt", "pdf", "rtf", "csv", "tsv", "tex",
            ],
        ),
        group(
            "Images",
            &[
                "png", "jpg", "jpeg", "gif", "bmp", "tif", "tiff", "webp", "svg", "ico", "heic",
            ],
        ),
        group(
            "Audio",
            &[
                "mp3", "wav", "flac", "aac", "ogg", "oga", "m4a", "wma", "opus", "mid", "midi",
            ],
        ),
        group(
            "Video",
            &[
                "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "ts",
            ],
        ),
        group(
            "Archives",
            &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "cab", "iso"],
        ),
        group(
            "Code",
            &[
                "rs", "py", "js", "ts", "jsx", "tsx", "c", "h", "cpp", "hpp", "cs", "java", "go",
                "rb", "php", "swift", "kt", "html", "css", "json", "xml", "yaml", "yml", "toml",
                "ini", "sql", "sh", "ps1", "bat", "lua",
            ],
        ),
    ]
}

/// Whole-application configuration, serialized as one JSON document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub roots: Vec<PathBuf>,
    #[serde(default)]
    pub db_path: Option<PathBuf>,
    #[serde(default)]
    pub window: WindowState,
    #[serde(default)]
    pub saved_searches: Vec<SavedSearch>,
    /// Most recent searches, newest first (capped at [`MAX_RECENTS`]).
    /// Full snapshots restore every filter; legacy bare strings (from
    /// older versions) restore just the name filter.
    #[serde(default)]
    pub recents: Vec<RecentEntry>,
    /// Default exclusion patterns (substring, or glob when the pattern
    /// contains `*` or `?`; same semantics as `Query::exclude`).
    #[serde(default)]
    pub exclusions: Vec<String>,
    #[serde(default)]
    pub presets: Vec<Preset>,
    /// Custom file groups; override [`builtin_file_groups`] by name
    /// (case-insensitive).
    #[serde(default)]
    pub file_groups: Vec<FileGroup>,
    /// CSV export delimiter remembered from the GUI picker (`,` default;
    /// `;`, tab, `|` for locales needing them). Old configs without the
    /// key load as comma.
    #[serde(default = "default_csv_delim")]
    pub csv_delim: char,
    /// GUI language code (`en`, `ro`). Old configs without the key
    /// load as English.
    #[serde(default = "default_language")]
    pub language: String,
    /// Interface scale multiplier (1.0 = 100%). Old configs without
    /// the key load unscaled.
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f32,
}

/// Default GUI language (English).
fn default_language() -> String {
    "en".to_string()
}

/// Default interface scale (100%).
fn default_ui_scale() -> f32 {
    1.0
}

/// Clamp a loaded scale into the supported range; non-finite values
/// mean 100%.
pub fn sanitize_ui_scale(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.5, 2.0)
    } else {
        1.0
    }
}

/// Default CSV delimiter (comma).
fn default_csv_delim() -> char {
    ','
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            db_path: None,
            window: WindowState::default(),
            saved_searches: Vec::new(),
            recents: Vec::new(),
            exclusions: Vec::new(),
            presets: Vec::new(),
            file_groups: Vec::new(),
            csv_delim: default_csv_delim(),
            language: default_language(),
            ui_scale: default_ui_scale(),
        }
    }
}

impl AppConfig {
    /// Resolve `name` to extensions: custom groups first, then
    /// [`builtin_file_groups`] (both case-insensitive). `None` means
    /// "no such group" — callers should error, not silently match all.
    pub fn resolve_file_group(&self, name: &str) -> Option<Vec<String>> {
        if let Some(custom) = self
            .file_groups
            .iter()
            .find(|g| g.name.eq_ignore_ascii_case(name))
        {
            return Some(custom.extensions.clone());
        }
        builtin_file_groups()
            .into_iter()
            .find(|g| g.name.eq_ignore_ascii_case(name))
            .map(|g| g.extensions)
    }

    /// Every group visible to pickers: customs first, then built-ins
    /// they do not override.
    pub fn all_file_groups(&self) -> Vec<FileGroup> {
        let mut out = self.file_groups.clone();
        for builtin in builtin_file_groups() {
            if !out
                .iter()
                .any(|g| g.name.eq_ignore_ascii_case(&builtin.name))
            {
                out.push(builtin);
            }
        }
        out
    }

    /// Insert or replace a custom group (matched by name,
    /// case-insensitive). Names and extensions are trimmed; empty ones
    /// are rejected so the stored list stays picker-ready.
    pub fn upsert_file_group(&mut self, group: FileGroup) -> Result<(), String> {
        let name = group.name.trim().to_string();
        if name.is_empty() {
            return Err("Group name is empty.".to_string());
        }
        let extensions: Vec<String> = group
            .extensions
            .iter()
            .map(|e| e.trim().trim_start_matches('.').to_string())
            .filter(|e| !e.is_empty())
            .collect();
        if extensions.is_empty() {
            return Err("Group needs at least one extension.".to_string());
        }
        let group = FileGroup { name, extensions };
        if let Some(slot) = self
            .file_groups
            .iter_mut()
            .find(|g| g.name.eq_ignore_ascii_case(&group.name))
        {
            *slot = group;
        } else {
            self.file_groups.push(group);
        }
        Ok(())
    }

    /// Delete a custom group by name (case-insensitive). Built-ins are
    /// not stored here, so removing one is a no-op `false`; deleting a
    /// custom that shadows a built-in reveals the built-in again.
    pub fn remove_file_group(&mut self, name: &str) -> bool {
        let before = self.file_groups.len();
        self.file_groups
            .retain(|g| !g.name.eq_ignore_ascii_case(name));
        self.file_groups.len() != before
    }
}

/// Errors from loading or saving an [`AppConfig`].
#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "config I/O: {e}"),
            Self::Json(e) => write!(f, "config parse: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

fn config_base_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SUPERSEARCH_CONFIG_DIR").map(PathBuf::from) {
        if !dir.as_os_str().is_empty() {
            return Some(dir);
        }
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(|base| PathBuf::from(base).join("supersearch"))
            .or_else(|| {
                std::env::var_os("USERPROFILE").map(|home| {
                    PathBuf::from(home)
                        .join("AppData")
                        .join("Roaming")
                        .join("supersearch")
                })
            })
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
            if !xdg.as_os_str().is_empty() {
                return Some(xdg.join("supersearch"));
            }
        }
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("supersearch"))
    }
}

/// Filesystem path of the default `config.json`.
pub fn default_config_path() -> PathBuf {
    config_base_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("config.json")
}

impl AppConfig {
    /// Load the config file at `path`.
    ///
    /// A missing file yields [`AppConfig::default`]; anything present must
    /// be valid JSON.
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e)),
            Ok(text) => {
                if text.trim().is_empty() {
                    return Ok(Self::default());
                }
                serde_json::from_str(&text).map_err(ConfigError::Json)
            }
        }
    }

    /// Save this config to `path`, creating parent directories as needed.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let text = serde_json::to_string_pretty(self).map_err(ConfigError::Json)?;
        fs::write(path, text).map_err(ConfigError::Io)?;
        Ok(())
    }

    /// Load from [`default_config_path`], falling back to the default on
    /// any error so startup never fails for config reasons.
    pub fn load() -> Self {
        Self::try_load().unwrap_or_default()
    }

    /// Load from [`default_config_path`], reporting errors.
    pub fn try_load() -> Result<Self, ConfigError> {
        Self::load_from(&default_config_path())
    }

    /// Save to [`default_config_path`].
    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&default_config_path())
    }

    /// Push a recent query description to the front, de-duplicating and
    /// capping the list at [`MAX_RECENTS`]. Empty strings are ignored.
    /// Stored as a [`RecentEntry::Legacy`] bare name (the CLI path).
    pub fn push_recent(&mut self, entry: impl Into<String>) {
        let entry = entry.into().trim().to_string();
        if entry.is_empty() {
            return;
        }
        self.push_entry(RecentEntry::Legacy(entry));
    }

    /// Push a full filter snapshot to the front, de-duplicating and
    /// capping the list at [`MAX_RECENTS`]. Empty (match-everything)
    /// snapshots are ignored — remembering them adds noise.
    pub fn push_recent_search(&mut self, saved: SavedSearch) {
        if saved == SavedSearch::default() {
            return;
        }
        self.push_entry(RecentEntry::Full(saved));
    }

    /// Front-insert with de-duplication and cap; the shared tail of both
    /// push paths.
    fn push_entry(&mut self, entry: RecentEntry) {
        self.recents.retain(|e| e != &entry);
        self.recents.insert(0, entry);
        self.recents.truncate(MAX_RECENTS);
    }

    /// Insert or replace the saved search with the same name.
    pub fn add_saved_search(&mut self, saved: SavedSearch) {
        if let Some(slot) = self
            .saved_searches
            .iter_mut()
            .find(|s| s.name == saved.name)
        {
            *slot = saved;
        } else {
            self.saved_searches.push(saved);
        }
    }

    /// Remove the saved search called `name`; `true` when one was removed.
    pub fn remove_saved_search(&mut self, name: &str) -> bool {
        let before = self.saved_searches.len();
        self.saved_searches.retain(|s| s.name != name);
        self.saved_searches.len() != before
    }

    /// Insert or replace the preset with the same name.
    pub fn upsert_preset(&mut self, preset: Preset) {
        if let Some(slot) = self.presets.iter_mut().find(|p| p.name == preset.name) {
            *slot = preset;
        } else {
            self.presets.push(preset);
        }
    }

    /// Remember the roots/db used by a scan or search so the next run can
    /// reuse them when its flags are omitted.
    pub fn record_usage(&mut self, roots: &[PathBuf], db_path: Option<&PathBuf>) {
        if !roots.is_empty() {
            self.roots = roots.to_vec();
        }
        if let Some(db) = db_path {
            self.db_path = Some(db.clone());
        }
    }
}

#[cfg(test)]
mod tests;

use std::fs::File;
use std::io::Read;
use std::path::Path;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use supersearch_core::FileEntry;

/// Upper bound on how many bytes of a file are read when evaluating the
/// [`Query::content`] filter.
///
/// Files larger than this are matched against their first
/// [`MAX_CONTENT_BYTES`] bytes only, so a single huge file cannot blow up
/// search latency or memory use.
pub const MAX_CONTENT_BYTES: u64 = 1_048_576;

/// Search filters over [`FileEntry`] values.
///
/// Every field is optional; an entry must satisfy **all** set fields to
/// match (logical AND). An empty (default) query matches everything.
///
/// The [`Query::or`] / [`Query::not`] fields nest sub-queries: when `or` is
/// non-empty at least one sub-query must match, and no `not` sub-query may
/// match. Both combine with AND against the rest of the fields.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    /// Case-sensitive substring that must appear in the entry path.
    pub name_substr: Option<String>,
    /// Regular expression that must match the entry path.
    ///
    /// An invalid expression never panics: it simply matches nothing.
    pub regex: Option<String>,
    /// Fuzzy pattern (nucleo matcher) matched against the file name,
    /// falling back to the full path when there is no file name.
    pub fuzzy: Option<String>,
    /// Substring that must appear in the file's contents, checked against
    /// at most the first [`MAX_CONTENT_BYTES`] bytes. Directories and
    /// unreadable files never match a content filter.
    pub content: Option<String>,
    /// Minimum file size in bytes (inclusive).
    pub min_size: Option<u64>,
    /// Maximum file size in bytes (inclusive).
    pub max_size: Option<u64>,
    /// Only entries modified strictly after this Unix timestamp match.
    pub modified_after: Option<i64>,
    /// Only entries modified strictly before this Unix timestamp match.
    pub modified_before: Option<i64>,
    /// Alias for [`Query::modified_before`].
    ///
    /// When both are set the entry must satisfy both (the earlier bound
    /// wins).
    pub before: Option<i64>,
    /// Wildcard pattern (`*` matches any run, `?` one char) matched
    /// against the entry path. `*` spans path separators, so `*.rs`
    /// matches `src/main.rs`.
    pub glob: Option<String>,
    /// Extension allow-list, e.g. `["rs", ".md"]`. Empty disables the
    /// filter. Leading dots are ignored and comparison is always
    /// case-insensitive, so `"RS"` matches `main.rs`.
    pub extensions: Vec<String>,
    /// Case-insensitive matching for the [`Query::name_substr`],
    /// [`Query::content`], [`Query::glob`], [`Query::regex`] and
    /// [`Query::exclude`] filters. Off by default (exact matching).
    pub case_insensitive: bool,
    /// Skip-list over the entry path: a matching entry never matches,
    /// even if every other filter passes. Each pattern is a substring,
    /// unless it contains `*` or `?`, in which case it is a glob
    /// (see [`Query::glob`]). Empty disables the filter.
    pub exclude: Vec<String>,
    /// OR group: when non-empty, at least one sub-query must match.
    /// Combines with AND against the other fields.
    pub or: Vec<Query>,
    /// NOT group: no sub-query may match. Combines with AND against the
    /// other fields.
    pub not: Vec<Query>,
}

impl Query {
    /// An empty query: matches every entry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Effective "modified before" bound, if either spelling is set.
    ///
    /// Returns the earlier of the two bounds when both are set.
    fn effective_before(&self) -> Option<i64> {
        match (self.modified_before, self.before) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// True when the query needs filesystem metadata or contents: size
    /// bounds, date bounds, or a content substring. Name-only queries
    /// (substring, regex, fuzzy, glob, extensions, exclusions) never do.
    pub fn needs_metadata(&self) -> bool {
        self.min_size.is_some()
            || self.max_size.is_some()
            || self.modified_after.is_some()
            || self.modified_before.is_some()
            || self.before.is_some()
            || self.content.is_some()
            || self.or.iter().any(Query::needs_metadata)
            || self.not.iter().any(Query::needs_metadata)
    }

    /// Filename-only prefilter: the [`Query::fuzzy`] and
    /// [`Query::extensions`] conjuncts, decidable without a resolved path
    /// or any filesystem I/O.
    ///
    /// Every entry passing [`Query::matches`] also passes this (it checks
    /// a subset of the same ANDed conjuncts against the same file name),
    /// so the MFT fast path can discard non-candidates before resolving
    /// parent FRNs; survivors always get the full check afterwards.
    pub fn matches_name(&self, file_name: &str) -> bool {
        if let Some(pattern) = &self.fuzzy {
            if !fuzzy_matches(file_name, pattern) {
                return false;
            }
        }
        if !ext_matches_name(file_name, &self.extensions) {
            return false;
        }
        true
    }

    /// Check every filter, including nested [`Query::or`] / [`Query::not`].
    ///
    /// Performs filesystem I/O when [`Query::content`] is set; returns
    /// `false` for directories and unreadable files in that case.
    pub fn matches(&self, entry: &FileEntry) -> bool {
        Prepared::new(self).test(entry)
    }
}

/// A query with its regex compiled and fuzzy pattern parsed once, so
/// [`search`] pays that cost per query instead of per entry.
struct Prepared<'q> {
    query: &'q Query,
    regex: Option<regex::Regex>,
    fuzzy: Option<Pattern>,
}

impl<'q> Prepared<'q> {
    fn new(query: &'q Query) -> Self {
        let regex = query
            .regex
            .as_deref()
            .and_then(|p| compile_regex(p, query.case_insensitive).ok());
        let fuzzy = query
            .fuzzy
            .as_deref()
            .map(|p| Pattern::parse(p, CaseMatching::Ignore, Normalization::Smart));
        Self {
            query,
            regex,
            fuzzy,
        }
    }

    /// `true` when the query has a regex filter but it failed to compile.
    fn regex_invalid(&self) -> bool {
        self.query.regex.is_some() && self.regex.is_none()
    }

    fn test(&self, entry: &FileEntry) -> bool {
        let query = self.query;
        let ci = query.case_insensitive;
        let path = entry.path.to_string_lossy();
        if let Some(needle) = &query.name_substr {
            if !contains_needle(&path, needle, ci) {
                return false;
            }
        }
        if query.regex.is_some() {
            match &self.regex {
                Some(re) => {
                    if !re.is_match(&path) {
                        return false;
                    }
                }
                // An invalid expression matches nothing, never panics.
                None => return false,
            }
        }
        if let Some(pattern) = &self.fuzzy {
            let target = fuzzy_target(entry);
            let mut matcher = Matcher::new(Config::DEFAULT);
            let mut buf = Vec::new();
            let haystack = Utf32Str::new(&target, &mut buf);
            if pattern.score(haystack, &mut matcher).is_none() {
                return false;
            }
        }
        if let Some(glob) = &query.glob {
            if !glob_matches_ci(&path, glob, ci) {
                return false;
            }
        }
        if !ext_matches(entry, &query.extensions) {
            return false;
        }
        if let Some(min) = query.min_size {
            if entry.size < min {
                return false;
            }
        }
        if let Some(max) = query.max_size {
            if entry.size > max {
                return false;
            }
        }
        if let Some(after) = query.modified_after {
            if entry.mtime <= after {
                return false;
            }
        }
        if let Some(before) = query.effective_before() {
            if entry.mtime >= before {
                return false;
            }
        }
        if let Some(needle) = &query.content {
            if !file_content_matches_ci(&entry.path, needle, ci) {
                return false;
            }
        }
        for pattern in &query.exclude {
            let hit = if pattern.contains('*') || pattern.contains('?') {
                glob_matches_ci(&path, pattern, ci)
            } else {
                contains_needle(&path, pattern, ci)
            };
            if hit {
                return false;
            }
        }
        if !query.or.is_empty() && !query.or.iter().any(|sub| sub.matches(entry)) {
            return false;
        }
        if query.not.iter().any(|sub| sub.matches(entry)) {
            return false;
        }
        true
    }
}

/// Substring check honoring the [`Query::case_insensitive`] toggle.
fn contains_needle(haystack: &str, needle: &str, case_insensitive: bool) -> bool {
    if case_insensitive {
        haystack.to_lowercase().contains(&needle.to_lowercase())
    } else {
        haystack.contains(needle)
    }
}

/// `true` when the entry's extension is in `exts`; always `true` when the
/// list is empty. Leading dots are ignored and comparison is
/// case-insensitive. Entries without an extension never match a non-empty
/// list.
fn ext_matches(entry: &FileEntry, exts: &[String]) -> bool {
    match entry.path.file_name().map(|n| n.to_string_lossy()) {
        Some(name) => ext_matches_name(&name, exts),
        // No file name (e.g. a bare volume root): only an empty list matches.
        None => exts.is_empty(),
    }
}

/// [`ext_matches`] against a bare file name (no parent path needed).
///
/// The MFT fast path calls this before parent FRNs resolve to full paths.
fn ext_matches_name(file_name: &str, exts: &[String]) -> bool {
    if exts.is_empty() {
        return true;
    }
    let actual = match Path::new(file_name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
    {
        Some(e) => e,
        None => return false,
    };
    exts.iter()
        .any(|e| e.trim_start_matches('.').to_lowercase() == actual)
}

/// Text the [`Query::fuzzy`] pattern is matched against: the file name when
/// the path has one, otherwise the full path.
fn fuzzy_target(entry: &FileEntry) -> String {
    match entry.path.file_name().map(|n| n.to_string_lossy()) {
        Some(name) => name.into_owned(),
        None => entry.path.to_string_lossy().into_owned(),
    }
}

/// Nucleo fuzzy match of `pattern` against `text` (case-insensitive).
///
/// An empty pattern matches everything; matching never fails, it only
/// reports match/no-match.
pub fn fuzzy_matches(text: &str, pattern: &str) -> bool {
    let mut matcher = Matcher::new(Config::DEFAULT);
    let parsed = Pattern::parse(pattern, CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    let haystack = Utf32Str::new(text, &mut buf);
    parsed.score(haystack, &mut matcher).is_some()
}

/// Regex match of `pattern` against `text`.
///
/// Returns `false` when the pattern does not compile, so callers never
/// need to handle regex errors or panics.
pub fn regex_matches(text: &str, pattern: &str) -> bool {
    regex_matches_ci(text, pattern, false)
}

/// [`regex_matches`] with a case-insensitive toggle: when `case_insensitive`
/// is set the pattern matches regardless of case.
pub fn regex_matches_ci(text: &str, pattern: &str, case_insensitive: bool) -> bool {
    match compile_regex(pattern, case_insensitive) {
        Ok(re) => re.is_match(text),
        Err(_) => false,
    }
}

/// `true` when `pattern` compiles as a regular expression.
///
/// The CLI uses this to warn about an invalid `--regex` (which otherwise
/// matches nothing silently); matching semantics are unchanged.
pub fn regex_is_valid(pattern: &str) -> bool {
    regex::Regex::new(pattern).is_ok()
}

/// Parse a date filter: either a Unix timestamp (`1700000000`) or a
/// calendar day (`2024-01-15`, interpreted as 00:00:00 UTC).
///
/// This is the single shared implementation used by both the CLI
/// (`--modified-after` / `--modified-before`) and the GUI date fields,
/// so both accept exactly the same inputs.
pub fn parse_mtime(value: &str) -> Result<i64, String> {
    if let Ok(ts) = value.parse::<i64>() {
        return Ok(ts);
    }
    let date_err = format!("invalid date {value:?}: expected a Unix timestamp or YYYY-MM-DD");
    let mut parts = value.splitn(3, '-');
    let (Some(y), Some(m), Some(d)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(date_err);
    };
    let (Ok(y), Ok(m), Ok(d)) = (y.parse::<i64>(), m.parse::<i64>(), d.parse::<i64>()) else {
        return Err(date_err);
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(date_err);
    }
    // Impossible calendar days (Feb 30, Apr 31, Feb 29 on common
    // years) are rejected instead of normalizing into March.
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let max_day = match m {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if d > max_day {
        return Err(date_err);
    }
    Ok(days_to_unix(y, m as u32, d as u32))
}

/// Compile `pattern`, prefixing `(?i)` when `case_insensitive` is set.
/// Returns the compile error instead of panicking.
fn compile_regex(pattern: &str, case_insensitive: bool) -> Result<regex::Regex, regex::Error> {
    if case_insensitive {
        regex::Regex::new(&format!("(?i){pattern}"))
    } else {
        regex::Regex::new(pattern)
    }
}

/// Days-from-civil (Howard Hinnant's algorithm) converted to a Unix
/// timestamp at 00:00:00 UTC. Avoids a chrono dependency for one format.
fn days_to_unix(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month as i64 + 9).rem_euclid(12);
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe - 719468) * 86_400
}

/// Wildcard match of `pattern` against `text`: `*` matches any (possibly
/// empty) run of characters — including path separators — and `?` matches
/// exactly one character. Every other character matches itself literally.
pub fn glob_matches(text: &str, pattern: &str) -> bool {
    glob_matches_ci(text, pattern, false)
}

/// [`glob_matches`] with a case-insensitive toggle.
pub fn glob_matches_ci(text: &str, pattern: &str, case_insensitive: bool) -> bool {
    let (text, pattern) = if case_insensitive {
        (text.to_lowercase(), pattern.to_lowercase())
    } else {
        (text.to_string(), pattern.to_string())
    };
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < text.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == text[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star = Some(pi);
            pi += 1;
            mark = ti;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// Decode a content window for substring search: UTF-8 (with or
/// without BOM) first, then UTF-16LE/BE (BOM-sniffed, else heuristic
/// when one byte of each pair is usually NUL), finally lossy UTF-8 so
/// binary files stay searchable without error.
pub fn decode_text_bytes(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(&bytes[3..]).into_owned();
    }
    if let [0xFF, 0xFE, rest @ ..] = bytes {
        return decode_utf16(rest, false);
    }
    if let [0xFE, 0xFF, rest @ ..] = bytes {
        return decode_utf16(rest, true);
    }
    if bytes.contains(&0) {
        // NUL is technically valid UTF-8, so it cannot decide the
        // encoding: sniff BOM-less UTF-16 first (ASCII-range text
        // with NUL on one side of 4/5 pairs). Binaries with scattered
        // NULs miss the bar and keep the old lossy path below.
        if bytes.len() >= 4 && bytes.len() % 2 == 0 {
            let pairs = bytes.chunks_exact(2).count();
            let lo_nul = bytes.chunks_exact(2).filter(|p| p[0] == 0).count();
            let hi_nul = bytes.chunks_exact(2).filter(|p| p[1] == 0).count();
            if hi_nul * 5 >= pairs * 4 {
                return decode_utf16(bytes, false);
            }
            if lo_nul * 5 >= pairs * 4 {
                return decode_utf16(bytes, true);
            }
        }
    } else if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Decode raw UTF-16 units (`big_endian` selects the byte order);
/// undecodable units become U+FFFD, a trailing odd byte is dropped.
fn decode_utf16(bytes: &[u8], big_endian: bool) -> String {
    let even = bytes.len() & !1;
    let units: Vec<u16> = bytes[..even]
        .chunks_exact(2)
        .map(|p| {
            if big_endian {
                u16::from_be_bytes([p[0], p[1]])
            } else {
                u16::from_le_bytes([p[0], p[1]])
            }
        })
        .collect();
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Check whether the file at `path` contains `needle` within its first
/// [`MAX_CONTENT_BYTES`] bytes.
///
/// Returns `false` for directories, missing files, and unreadable files.
/// The byte window is decoded with [`decode_text_bytes`], so UTF-16
/// files match too and binary files stay searchable without error.
pub fn file_content_matches(path: &Path, needle: &str) -> bool {
    file_content_matches_ci(path, needle, false)
}

/// [`file_content_matches`] with a case-insensitive toggle.
pub fn file_content_matches_ci(path: &Path, needle: &str, case_insensitive: bool) -> bool {
    if path.is_dir() {
        return false;
    }
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    // `take` caps the read; a short read just yields fewer bytes.
    let bytes: Vec<u8> = match file
        .take(MAX_CONTENT_BYTES)
        .bytes()
        .take(MAX_CONTENT_BYTES as usize)
        .collect()
    {
        Ok(b) => b,
        Err(_) => return false,
    };
    let text = decode_text_bytes(&bytes);
    if case_insensitive {
        text.to_lowercase().contains(&needle.to_lowercase())
    } else {
        text.contains(needle)
    }
}

/// Filter `entries` down to those satisfying `query`.
///
/// The regex is compiled once and the fuzzy pattern parsed once; entries
/// failing cheap metadata filters short-circuit before any content I/O.
/// An invalid [`Query::regex`] matches nothing, so the result is empty
/// rather than an error.
pub fn search<'a>(entries: &'a [FileEntry], query: &Query) -> Vec<&'a FileEntry> {
    let prepared = Prepared::new(query);
    if prepared.regex_invalid() {
        return Vec::new();
    }
    entries
        .iter()
        .filter(|entry| prepared.test(entry))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(path: &str, size: u64, mtime: i64) -> FileEntry {
        FileEntry::new(PathBuf::from(path), size, mtime, false)
    }

    fn fixture() -> Vec<FileEntry> {
        vec![
            entry("/proj/src/main.rs", 1200, 1_700_000_000),
            entry("/proj/src/lib.rs", 800, 1_700_000_100),
            entry("/proj/README.md", 300, 1_700_000_200),
            entry("/proj/target/debug/app", 5_000_000, 1_700_000_300),
        ]
    }

    #[test]
    fn empty_query_matches_everything() {
        let entries = fixture();
        let q = Query::new();
        assert_eq!(search(&entries, &q).len(), 4);
        assert!(entries.iter().all(|e| q.matches(e)));
    }

    #[test]
    fn name_prefilter_covers_fuzzy_and_extensions() {
        let q = Query {
            fuzzy: Some("mrs".to_string()),
            extensions: vec!["rs".to_string()],
            ..Query::default()
        };
        assert!(q.matches_name("main.rs"));
        assert!(!q.matches_name("main.md"));
        assert!(!q.matches_name("notes.txt"));
        // No fuzzy/extensions set: everything passes the prefilter.
        assert!(Query::new().matches_name("anything.bin"));
    }

    #[test]
    fn needs_metadata_covers_size_date_content_and_nesting() {
        assert!(!Query::new().needs_metadata());
        for q in [
            Query {
                name_substr: Some("x".into()),
                ..Query::default()
            },
            Query {
                regex: Some("x".into()),
                ..Query::default()
            },
            Query {
                fuzzy: Some("x".into()),
                ..Query::default()
            },
            Query {
                glob: Some("*.rs".into()),
                ..Query::default()
            },
            Query {
                extensions: vec!["rs".into()],
                ..Query::default()
            },
            Query {
                exclude: vec!["t".into()],
                ..Query::default()
            },
        ] {
            assert!(!q.needs_metadata());
        }
        for q in [
            Query {
                min_size: Some(1),
                ..Query::default()
            },
            Query {
                max_size: Some(1),
                ..Query::default()
            },
            Query {
                modified_after: Some(1),
                ..Query::default()
            },
            Query {
                modified_before: Some(1),
                ..Query::default()
            },
            Query {
                before: Some(1),
                ..Query::default()
            },
            Query {
                content: Some("x".into()),
                ..Query::default()
            },
            Query {
                or: vec![Query {
                    min_size: Some(1),
                    ..Query::default()
                }],
                ..Query::default()
            },
            Query {
                not: vec![Query {
                    content: Some("x".into()),
                    ..Query::default()
                }],
                ..Query::default()
            },
        ] {
            assert!(q.needs_metadata());
        }
    }

    #[test]
    fn full_matches_imply_name_prefilter() {
        // The prefilter checks a subset of the ANDed conjuncts, so any
        // entry passing the full query must pass it too: dropping
        // prefilter survivors can never lose a hit.
        let queries = vec![
            Query::new(),
            Query {
                name_substr: Some("src".to_string()),
                ..Query::default()
            },
            Query {
                fuzzy: Some("mrs".to_string()),
                extensions: vec!["rs".to_string()],
                min_size: Some(100),
                content: Some("fn".to_string()),
                ..Query::default()
            },
            Query {
                glob: Some("*.md".to_string()),
                exclude: vec!["target".to_string()],
                ..Query::default()
            },
        ];
        for entry in fixture() {
            let Some(name) = entry.path.file_name().map(|n| n.to_string_lossy()) else {
                continue;
            };
            for q in &queries {
                if q.matches(&entry) {
                    assert!(
                        q.matches_name(&name),
                        "full hit must pass prefilter: {entry:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn name_substr_filters_paths() {
        let entries = fixture();
        let q = Query {
            name_substr: Some("src".to_string()),
            ..Query::default()
        };
        let hits = search(&entries, &q);
        assert_eq!(hits.len(), 2);
        assert!(hits
            .iter()
            .all(|e| e.path.to_string_lossy().contains("src")));
    }

    #[test]
    fn size_filters_are_inclusive() {
        let entries = fixture();
        let q = Query {
            min_size: Some(800),
            max_size: Some(1200),
            ..Query::default()
        };
        let hits = search(&entries, &q);
        assert_eq!(hits.len(), 2);
        // Boundaries themselves match.
        assert!(q.matches(&entry("/x", 800, 0)));
        assert!(q.matches(&entry("/x", 1200, 0)));
        assert!(!q.matches(&entry("/x", 799, 0)));
        assert!(!q.matches(&entry("/x", 1201, 0)));
    }

    #[test]
    fn mtime_filters_use_strict_inequality() {
        let entries = fixture();
        let q = Query {
            modified_after: Some(1_700_000_000),
            modified_before: Some(1_700_000_300),
            ..Query::default()
        };
        let hits = search(&entries, &q);
        assert_eq!(hits.len(), 2);
        // Exact bounds are excluded.
        assert!(!q.matches(&entry("/x", 0, 1_700_000_000)));
        assert!(!q.matches(&entry("/x", 0, 1_700_000_300)));
    }

    #[test]
    fn before_alias_behaves_like_modified_before() {
        let q = Query {
            before: Some(1_700_000_200),
            ..Query::default()
        };
        assert!(q.matches(&entry("/x", 0, 1_700_000_100)));
        assert!(!q.matches(&entry("/x", 0, 1_700_000_200)));
        // Both spellings set: the earlier bound wins.
        let q2 = Query {
            modified_before: Some(1_700_000_300),
            before: Some(1_700_000_100),
            ..Query::default()
        };
        assert!(q2.matches(&entry("/x", 0, 1_700_000_050)));
        assert!(!q2.matches(&entry("/x", 0, 1_700_000_150)));
    }

    #[test]
    fn regex_matches_paths() {
        let entries = fixture();
        let q = Query {
            regex: Some(r"src/.*\.rs$".to_string()),
            ..Query::default()
        };
        assert_eq!(search(&entries, &q).len(), 2);
    }

    #[test]
    fn invalid_regex_matches_nothing_without_panicking() {
        let entries = fixture();
        let q = Query {
            regex: Some("([invalid".to_string()),
            ..Query::default()
        };
        assert!(search(&entries, &q).is_empty());
        assert!(!q.matches(&entries[0]));
        assert!(!regex_matches("anything", "([invalid"));
    }

    #[test]
    fn fuzzy_matches_filename_subsequence() {
        assert!(fuzzy_matches("main.rs", "mrs"));
        assert!(fuzzy_matches("main.rs", "MAIN"));
        assert!(!fuzzy_matches("main.rs", "zzz"));

        let entries = fixture();
        let q = Query {
            fuzzy: Some("mrs".to_string()),
            ..Query::default()
        };
        let hits = search(&entries, &q);
        assert!(hits.iter().any(|e| e.path.ends_with("main.rs")));
        assert!(hits.iter().all(|e| q.matches(e)));
    }

    #[test]
    fn combined_filters_intersect() {
        let entries = fixture();
        let q = Query {
            name_substr: Some("src".to_string()),
            regex: Some(r"\.rs$".to_string()),
            min_size: Some(1000),
            ..Query::default()
        };
        let hits = search(&entries, &q);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("main.rs"));
    }

    fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "supersearch-query-test-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::write(&path, bytes).expect("write temp file");
        path
    }

    #[test]
    fn content_matches_within_cap() {
        let path = temp_file("hello.txt", b"hello needle world");
        let found = FileEntry::new(path.clone(), 18, 0, false);
        let q = Query {
            content: Some("needle".to_string()),
            ..Query::default()
        };
        assert!(q.matches(&found));
        assert_eq!(search(std::slice::from_ref(&found), &q).len(), 1);
        let missing = Query {
            content: Some("absent".to_string()),
            ..Query::default()
        };
        assert!(!missing.matches(&found));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn decode_text_bytes_handles_boms_and_utf16() {
        assert_eq!(decode_text_bytes(b"plain"), "plain");
        assert_eq!(decode_text_bytes(&[0xEF, 0xBB, 0xBF, b'h', b'i']), "hi");
        let units: Vec<u16> = "revenue up".encode_utf16().collect();
        let mut le = vec![0xFF, 0xFE];
        let mut be = vec![0xFE, 0xFF];
        let mut raw: Vec<u8> = Vec::new();
        for u in &units {
            le.extend_from_slice(&u.to_le_bytes());
            be.extend_from_slice(&u.to_be_bytes());
            raw.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_text_bytes(&le), "revenue up");
        assert_eq!(decode_text_bytes(&be), "revenue up");
        // BOM-less Notepad-style LE still decodes via heuristic.
        assert_eq!(decode_text_bytes(&raw), "revenue up");
        // Binary junk: no match, no panic.
        assert!(!decode_text_bytes(&[0xFF, 0x00, 0x89, 0x50, 0x00]).contains("revenue"));
    }

    #[test]
    fn content_matches_utf16_files() {
        let mut bytes = vec![0xFF, 0xFE];
        for u in "quarterly revenue up".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let path = temp_file("utf16.txt", &bytes);
        let entry = FileEntry::new(path.clone(), bytes.len() as u64, 0, false);
        let q = Query {
            content: Some("revenue".to_string()),
            ..Query::default()
        };
        assert!(q.matches(&entry));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn content_beyond_cap_does_not_match() {
        // Needle placed past the cap must not match: only the first
        // MAX_CONTENT_BYTES are inspected.
        let mut bytes = vec![b'a'; (MAX_CONTENT_BYTES + 64) as usize];
        bytes.extend_from_slice(b"needle");
        let path = temp_file("big.bin", &bytes);
        let big = FileEntry::new(path.clone(), bytes.len() as u64, 0, false);
        let q = Query {
            content: Some("needle".to_string()),
            ..Query::default()
        };
        assert!(!q.matches(&big));
        assert!(search(std::slice::from_ref(&big), &q).is_empty());

        // Same needle inside the cap matches.
        let mut bytes2 = Vec::from(b"needle".as_slice());
        bytes2.resize((MAX_CONTENT_BYTES + 64) as usize, b'a');
        let path2 = temp_file("big2.bin", &bytes2);
        let big2 = FileEntry::new(path2.clone(), bytes2.len() as u64, 0, false);
        assert!(q.matches(&big2));
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&path2).ok();
    }

    #[test]
    fn or_group_matches_either_branch() {
        let entries = fixture();
        let q = Query {
            or: vec![
                Query {
                    name_substr: Some("README".to_string()),
                    ..Query::default()
                },
                Query {
                    name_substr: Some("target".to_string()),
                    ..Query::default()
                },
            ],
            ..Query::default()
        };
        let hits = search(&entries, &q);
        assert_eq!(hits.len(), 2);
        assert!(q.matches(&entry("/proj/README.md", 0, 0)));
        assert!(q.matches(&entry("/proj/target/debug/app", 0, 0)));
        assert!(!q.matches(&entry("/proj/src/main.rs", 0, 0)));
        // Empty `or` disables the filter.
        assert!(Query::new().matches(&entry("/anything", 0, 0)));
    }

    #[test]
    fn or_combines_with_and_against_other_filters() {
        let entries = fixture();
        let q = Query {
            extensions: vec!["rs".to_string()],
            or: vec![
                Query {
                    name_substr: Some("main".to_string()),
                    ..Query::default()
                },
                Query {
                    name_substr: Some("lib".to_string()),
                    ..Query::default()
                },
            ],
            ..Query::default()
        };
        let hits = search(&entries, &q);
        assert_eq!(hits.len(), 2);
        // README satisfies `or` but not the extension filter.
        assert!(!q.matches(&entry("/proj/README.md", 300, 0)));
    }

    #[test]
    fn not_group_excludes_matching_entries() {
        let entries = fixture();
        let q = Query {
            not: vec![Query {
                name_substr: Some("target".to_string()),
                ..Query::default()
            }],
            ..Query::default()
        };
        let hits = search(&entries, &q);
        assert_eq!(hits.len(), 3);
        assert!(!q.matches(&entry("/proj/target/debug/app", 0, 0)));
        assert!(q.matches(&entry("/proj/src/main.rs", 0, 0)));
        // Several `not` branches union their exclusions.
        let q2 = Query {
            not: vec![
                Query {
                    extensions: vec!["rs".to_string()],
                    ..Query::default()
                },
                Query {
                    extensions: vec!["md".to_string()],
                    ..Query::default()
                },
            ],
            ..Query::default()
        };
        assert_eq!(search(&entries, &q2).len(), 1);
    }

    #[test]
    fn extensions_filter_allow_list() {
        let entries = fixture();
        let q = Query {
            extensions: vec!["rs".to_string()],
            ..Query::default()
        };
        assert_eq!(search(&entries, &q).len(), 2);
        // Leading dots are ignored and matching is case-insensitive.
        let q2 = Query {
            extensions: vec![".RS".to_string(), "md".to_string()],
            ..Query::default()
        };
        assert_eq!(search(&entries, &q2).len(), 3);
        // Entries without an extension never match a non-empty list.
        assert!(!q.matches(&entry("/proj/Makefile", 10, 0)));
        // Empty list disables the filter.
        assert!(Query::new().matches(&entry("/proj/Makefile", 10, 0)));
    }

    #[test]
    fn glob_star_and_question_mark() {
        assert!(glob_matches("src/main.rs", "*.rs"));
        assert!(glob_matches("src/main.rs", "src/*.rs"));
        assert!(glob_matches("src/main.rs", "src/mai?.rs"));
        assert!(!glob_matches("src/main.rs", "src/mai?.r"));
        assert!(!glob_matches("src/main.rs", "*.md"));
        assert!(glob_matches("anything", "*"));
        assert!(!glob_matches("", "?"));

        let entries = fixture();
        let q = Query {
            glob: Some("*.rs".to_string()),
            ..Query::default()
        };
        let hits = search(&entries, &q);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|e| q.matches(e)));
        let q2 = Query {
            glob: Some("/proj/src/?ib.rs".to_string()),
            ..Query::default()
        };
        assert_eq!(search(&entries, &q2).len(), 1);
    }

    #[test]
    fn case_insensitive_toggle() {
        let case_sensitive = Query {
            name_substr: Some("README".to_string()),
            ..Query::default()
        };
        assert!(!case_sensitive.matches(&entry("/proj/readme.md", 0, 0)));
        let folded = Query {
            name_substr: Some("README".to_string()),
            case_insensitive: true,
            ..Query::default()
        };
        assert!(folded.matches(&entry("/proj/readme.md", 0, 0)));
        assert!(folded.matches(&entry("/proj/README.md", 0, 0)));
        // Toggle also applies to globs and regexes.
        assert!(glob_matches_ci("SRC/MAIN.RS", "src/*.rs", true));
        assert!(!glob_matches("SRC/MAIN.RS", "src/*.rs"));
        assert!(regex_matches_ci("Hello", "^hello$", true));
        assert!(!regex_matches("Hello", "^hello$"));
        // And to file contents.
        let path = temp_file("ci.txt", b"Hello World");
        let found = FileEntry::new(path.clone(), 11, 0, false);
        assert!(!file_content_matches(&path, "hello"));
        assert!(file_content_matches_ci(&path, "hello", true));
        let q = Query {
            content: Some("hello".to_string()),
            case_insensitive: true,
            ..Query::default()
        };
        assert!(q.matches(&found));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn exclude_patterns_skip_matches() {
        let entries = fixture();
        let q = Query {
            exclude: vec!["target".to_string()],
            ..Query::default()
        };
        let hits = search(&entries, &q);
        assert_eq!(hits.len(), 3);
        assert!(!q.matches(&entry("/proj/target/debug/app", 0, 0)));
        // Glob excludes work too, and beat other filters.
        let q2 = Query {
            name_substr: Some("src".to_string()),
            exclude: vec!["*.rs".to_string()],
            ..Query::default()
        };
        assert!(search(&entries, &q2).is_empty());
        // Empty skip-list disables the filter.
        assert_eq!(search(&entries, &Query::new()).len(), 4);
    }

    #[test]
    fn new_filters_keep_existing_and_semantics() {
        let entries = fixture();
        let q = Query {
            name_substr: Some("src".to_string()),
            glob: Some("*.rs".to_string()),
            extensions: vec!["rs".to_string()],
            min_size: Some(1000),
            not: vec![Query {
                name_substr: Some("lib".to_string()),
                ..Query::default()
            }],
            ..Query::default()
        };
        let hits = search(&entries, &q);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("main.rs"));
    }

    #[test]
    fn parse_mtime_accepts_timestamps_and_dates() {
        assert_eq!(parse_mtime("1700000000"), Ok(1_700_000_000));
        assert_eq!(parse_mtime("0"), Ok(0));
        // 2024-01-15 is 00:00:00 UTC (2024-01-01 is 1704067200).
        assert_eq!(parse_mtime("2024-01-15"), Ok(1_705_276_800));
        assert_eq!(parse_mtime("1970-01-01"), Ok(0));
        // Invalid inputs error instead of panicking.
        assert!(parse_mtime("not-a-date-at-all!!!").is_err());
        assert!(parse_mtime("2024-13-01").is_err());
        assert!(parse_mtime("2024-01").is_err());
        assert!(parse_mtime("").is_err());
        // Impossible calendar days are rejected, not normalized.
        assert!(parse_mtime("2024-02-30").is_err());
        assert!(parse_mtime("2023-02-29").is_err());
        assert!(parse_mtime("2024-04-31").is_err());
        assert!(parse_mtime("2024-02-29").is_ok());
        assert!(parse_mtime("2024-04-30").is_ok());
    }

    #[test]
    fn regex_is_valid_reports_compilability() {
        assert!(regex_is_valid(r"src/.*\.rs$"));
        assert!(!regex_is_valid("([invalid"));
        assert!(regex_is_valid(""));
    }

    #[test]
    fn content_never_matches_dirs_or_missing_files() {
        let dir = FileEntry::new(PathBuf::from("/no/such/dir"), 0, 0, true);
        let missing = FileEntry::new(
            std::env::temp_dir().join("supersearch-query-test-missing-file.txt"),
            10,
            0,
            false,
        );
        let q = Query {
            content: Some("x".to_string()),
            ..Query::default()
        };
        assert!(!q.matches(&dir));
        assert!(!missing.path.exists());
        assert!(!q.matches(&missing));
    }
}

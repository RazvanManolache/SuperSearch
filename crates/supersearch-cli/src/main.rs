use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use supersearch_config::{default_config_path, AppConfig};
use supersearch_core::FileEntry;
#[cfg(windows)]
use supersearch_index::{
    index_fill_mft,
    platform::windows::{volume_for_root, volume_root_path},
    usn_tick, UsnTickOutcome,
};
use supersearch_index::{
    init_db, load_db, scan_roots, scan_to_db, spawn_index_thread, ScanOptions,
};
use supersearch_query::{parse_mtime, regex_is_valid, search, Query};

/// SuperSearch: fast parallel file finder with regex, fuzzy, content,
/// size, and date filters.
#[derive(Debug, Parser)]
#[command(name = "supersearch", version)]
struct Cli {
    /// Config file (JSON). Defaults to the platform config dir.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Walk roots and print (or store) every entry found.
    Scan(ScanArgs),
    /// Search on-demand scans and/or a stored index.
    Search(SearchArgs),
    /// Open a file or directory with its default application.
    Open(PathArgs),
    /// Reveal a file or directory in the system file manager.
    Reveal(PathArgs),
    /// Copy a (canonicalized) path to the system clipboard.
    CopyPath(PathArgs),
    /// Inspect or reset the persisted JSON config.
    Config(ConfigArgs),
    /// Maintain a USN-driven index in the background (Windows local
    /// volumes read deltas; everything else re-polls on the interval).
    Daemon(DaemonArgs),
}

/// Keep an index fresh: full fill where no cursor exists, journal
/// deltas elsewhere, walk-polling for the rest.
#[derive(Debug, Args)]
struct DaemonArgs {
    /// Scan roots (repeatable). Defaults to remembered roots.
    #[arg(long)]
    root: Vec<PathBuf>,
    /// Index database path. Defaults to the remembered one.
    #[arg(long)]
    db: Option<PathBuf>,
    /// Tick interval in seconds.
    #[arg(long, default_value_t = 5)]
    interval_secs: u64,
}

/// One filesystem path to act on (`open`, `reveal`).
#[derive(Debug, Args)]
struct PathArgs {
    /// File or directory to act on.
    path: PathBuf,
}

/// Output rendering for `search` hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    /// One path per line (default).
    Text,
    /// `path,size,mtime,is_dir` rows with a header line.
    Csv,
    /// JSON array of `{path,size,mtime,is_dir}` objects.
    Json,
    /// Standalone HTML document with one table row per hit.
    Html,
    /// RTF document, one paragraph per hit.
    Rtf,
    /// XML Spreadsheet 2003 workbook (opens in Excel).
    Xlsx,
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    action: ConfigAction,
}

#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Print the config file path and its JSON contents.
    Show,
    /// Print only the config file path.
    Path,
    /// Overwrite the config file with defaults.
    Reset,
}

#[derive(Debug, Args)]
struct ScanArgs {
    /// Root directories to walk recursively.
    /// When omitted, the roots remembered in the config file are used.
    roots: Vec<PathBuf>,
    /// SQLite index file to write. When omitted, entries go to stdout.
    #[arg(long)]
    db: Option<PathBuf>,
    /// Follow symbolic links while walking.
    #[arg(long, default_value_t = false)]
    follow_symlinks: bool,
    /// Include hidden files (skipped by default).
    #[arg(long, default_value_t = false)]
    include_hidden: bool,
}

#[derive(Debug, Args)]
struct SearchArgs {
    /// Root directories to scan on demand (repeatable).
    #[arg(long)]
    root: Vec<PathBuf>,
    /// SQLite index file from a previous `scan --db` (indexed mode).
    /// When both `--db` and `--root` are given, both sources are combined.
    /// Ignored together with the remembered DB under `--live`.
    #[arg(long)]
    db: Option<PathBuf>,
    /// Skip every index (explicit `--db` and remembered) and scan the
    /// roots on demand instead. Needs `--root`. Conflicts with `--fresh`.
    #[arg(long, conflicts_with = "fresh")]
    live: bool,
    /// Refresh the index for these roots before searching it: one
    /// daemon-equivalent pass (delta tick per local volume, full fill
    /// where the journal wrapped or no cursor exists, walk re-poll
    /// elsewhere). Needs `--db` and `--root`. Conflicts with `--live`.
    #[arg(long, conflicts_with = "live")]
    fresh: bool,
    /// Case-sensitive substring matched against the entry path.
    #[arg(long)]
    name: Option<String>,
    /// Regular expression matched against the entry path.
    /// An invalid expression matches nothing.
    #[arg(long)]
    regex: Option<String>,
    /// Fuzzy pattern matched against the file name (nucleo matcher).
    #[arg(long)]
    fuzzy: Option<String>,
    /// Substring that must appear in the file contents (first 1 MiB).
    #[arg(long)]
    content: Option<String>,
    /// Wildcard pattern matched against the entry path (`*` spans
    /// separators, `?` matches one char).
    #[arg(long)]
    glob: Option<String>,
    /// Extension allow-list, e.g. `--ext rs --ext md` (repeatable;
    /// leading dots ignored, always case-insensitive).
    #[arg(long)]
    ext: Vec<String>,
    /// File group allow-list, e.g. `--group Documents` (repeatable).
    /// Groups come from the config (`file_groups`, overriding the
    /// built-ins: Documents, Images, Audio, Video, Archives, Code) and
    /// merge into `--ext`. Unknown names are an error.
    #[arg(long)]
    group: Vec<String>,
    /// Case-insensitive matching for name, content, glob, regex and
    /// exclude filters.
    #[arg(long, default_value_t = false)]
    ignore_case: bool,
    /// Minimum file size in bytes (inclusive).
    #[arg(long)]
    min_size: Option<u64>,
    /// Maximum file size in bytes (inclusive).
    #[arg(long)]
    max_size: Option<u64>,
    /// Only entries modified after this time (Unix timestamp or YYYY-MM-DD).
    #[arg(long)]
    modified_after: Option<String>,
    /// Only entries modified before this time (Unix timestamp or YYYY-MM-DD).
    #[arg(long)]
    modified_before: Option<String>,
    /// Follow symbolic links while scanning on-demand roots.
    #[arg(long, default_value_t = false)]
    follow_symlinks: bool,
    /// Include hidden files in on-demand scans (skipped by default).
    #[arg(long, default_value_t = false)]
    include_hidden: bool,
    /// Maximum hits to print (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    limit: usize,
    /// Extra exclusion patterns (repeatable; merged with the config
    /// exclusions). Same semantics as `Query::exclude`: a substring,
    /// or a glob when the pattern contains `*` or `?`.
    #[arg(long)]
    exclude: Vec<String>,
    /// OR substring: a hit needs ANY of these (repeatable). Each term
    /// is a case-sensitivity-honoring substring like `--name`, combined
    /// with AND against the other flags.
    #[arg(long = "or")]
    or_terms: Vec<String>,
    /// NOT substring: a hit needs NONE of these (repeatable). Same
    /// matching rule as `--or`, inverted.
    #[arg(long = "not")]
    not_terms: Vec<String>,
    /// How hits are printed: `text` (paths), `csv`, or `json` export.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    format: OutputFormat,
    /// CSV field delimiter: a single character or one of `comma`,
    /// `semicolon`, `tab`, `pipe`. Only applies to `--format csv`.
    #[arg(long, value_parser = parse_csv_delim)]
    csv_delim: Option<char>,
}

/// Parse a `--csv-delim` value: one literal character or a common name.
fn parse_csv_delim(s: &str) -> Result<char, String> {
    match s {
        "comma" => Ok(','),
        "semicolon" => Ok(';'),
        "tab" => Ok('\t'),
        "pipe" => Ok('|'),
        _ => {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                // Quoting, newlines, and non-ASCII have no business as
                // delimiters: the byte-level `csv` writer and the core
                // renderer both assume one ASCII separator byte.
                (Some(c), None) if c.is_ascii() && !c.is_ascii_control() && c != '"' => Ok(c),
                _ => Err(format!(
                    "expected an ASCII delimiter or comma/semicolon/tab/pipe, got {s:?}"
                )),
            }
        }
    }
}

/// Build the runnable [`Query`] for `search` from its flags.
///
/// `--exclude` flags merge with (never replace) the config exclusions.
fn build_search_query(args: &SearchArgs, config_exclusions: &[String]) -> Result<Query, String> {
    let mut exclude: Vec<String> = config_exclusions.to_vec();
    exclude.extend(args.exclude.iter().cloned());
    // OR/NOT terms become single-substring subqueries — the same shape
    // the GUI builds from its Any-of/None-of fields.
    let sub = |terms: &[String]| {
        terms
            .iter()
            .map(|t| Query {
                name_substr: Some(t.clone()),
                ..Query::default()
            })
            .collect()
    };
    Ok(Query {
        name_substr: args.name.clone(),
        regex: args.regex.clone(),
        fuzzy: args.fuzzy.clone(),
        content: args.content.clone(),
        glob: args.glob.clone(),
        extensions: args.ext.clone(),
        case_insensitive: args.ignore_case,
        min_size: args.min_size,
        max_size: args.max_size,
        modified_after: opt_mtime(&args.modified_after, "--modified-after")?,
        modified_before: opt_mtime(&args.modified_before, "--modified-before")?,
        exclude,
        or: sub(&args.or_terms),
        not: sub(&args.not_terms),
        ..Query::default()
    })
}

/// Parse an optional date filter with the shared
/// [`supersearch_query::parse_mtime`] implementation (Unix timestamp or
/// `YYYY-MM-DD`), so the CLI and GUI accept exactly the same inputs.
fn opt_mtime(value: &Option<String>, flag: &str) -> Result<Option<i64>, String> {
    value
        .as_deref()
        .map(parse_mtime)
        .transpose()
        .map_err(|e| format!("{flag}: {e}"))
}

/// Spinner updated from the index crate's progress callback.
fn progress_bar() -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(80));
    pb
}

/// Resolve the config path: `--config` wins, otherwise the platform default.
fn resolve_config_path(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(default_config_path)
}

/// Load config for a run; a missing or unreadable file yields defaults so
/// the command still works (a warning goes to stderr).
fn load_config_for_run(path: &Path) -> AppConfig {
    match AppConfig::load_from(path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!(
                "supersearch: warning: config {}: {e}; using defaults",
                path.display()
            );
            AppConfig::default()
        }
    }
}

/// Persist config after a run; failures warn but never fail the command.
fn save_config_after_run(path: &Path, config: &AppConfig) {
    if let Err(e) = config.save_to(path) {
        eprintln!("supersearch: warning: save config {}: {e}", path.display());
    }
}

fn run_scan(args: ScanArgs, config_path: &Path) -> Result<(), String> {
    let mut config = load_config_for_run(config_path);
    let roots = if args.roots.is_empty() {
        config.roots.clone()
    } else {
        args.roots.clone()
    };
    if roots.is_empty() {
        return Err("pass at least one root directory or set roots in the config file".to_string());
    }
    // A `--db` flag wins; otherwise reuse the remembered index path.
    let db_path = args.db.clone().or_else(|| config.db_path.clone());
    let options = ScanOptions {
        roots: roots.clone(),
        follow_symlinks: args.follow_symlinks,
        skip_hidden: !args.include_hidden,
    };
    let pb = progress_bar();
    if let Some(db_path) = db_path {
        let mut conn = rusqlite::Connection::open(&db_path)
            .map_err(|e| format!("open db {}: {e}", db_path.display()))?;
        init_db(&conn).map_err(|e| format!("init db: {e}"))?;
        let pb2 = pb.clone();
        let progress = move |n: u64| pb2.set_message(format!("{n} files indexed"));
        let stats =
            scan_to_db(&mut conn, &options, Some(&progress)).map_err(|e| format!("scan: {e}"))?;
        pb.finish_with_message(format!(
            "{} files indexed ({} skipped, {} errors)",
            stats.scanned, stats.skipped, stats.errors
        ));
    } else {
        let pb2 = pb.clone();
        let progress = move |n: u64| pb2.set_message(format!("{n} files scanned"));
        let result = scan_roots(&options, Some(&progress));
        pb.finish_and_clear();
        for e in &result.entries {
            println!("{}", e.path.display());
        }
        eprintln!(
            "{} files scanned ({} skipped, {} errors)",
            result.entries.len(),
            result.skipped,
            result.errors
        );
    }
    config.record_usage(&roots, args.db.as_ref());
    // Remember the explicit `--db` flag even for stdout scans so a later
    // `search` without flags can reuse it.
    if config.db_path.is_none() {
        config.db_path = args.db.clone();
    }
    save_config_after_run(config_path, &config);
    Ok(())
}

fn describe_search(args: &SearchArgs) -> String {
    let mut parts = Vec::new();
    if let Some(n) = &args.name {
        parts.push(format!("name:{n}"));
    }
    if let Some(r) = &args.regex {
        parts.push(format!("regex:{r}"));
    }
    if let Some(f) = &args.fuzzy {
        parts.push(format!("fuzzy:{f}"));
    }
    if let Some(c) = &args.content {
        parts.push(format!("content:{c}"));
    }
    if let Some(g) = &args.glob {
        parts.push(format!("glob:{g}"));
    }
    for e in &args.ext {
        parts.push(format!("ext:{e}"));
    }
    for g in &args.group {
        parts.push(format!("group:{g}"));
    }
    for o in &args.or_terms {
        parts.push(format!("any:{o}"));
    }
    for n in &args.not_terms {
        parts.push(format!("not:{n}"));
    }
    if args.ignore_case {
        parts.push("case:insensitive".to_string());
    }
    if args.live {
        parts.push("mode:live".to_string());
    }
    if args.fresh {
        parts.push("mode:fresh".to_string());
    }
    if parts.is_empty() {
        parts.push("(all)".to_string());
    }
    parts.join(" ")
}

/// Render one entry as a JSON object for `--format json`.
fn entry_to_json(entry: &FileEntry) -> serde_json::Value {
    serde_json::json!({
        "path": entry.path.display().to_string(),
        "size": entry.size,
        "mtime": entry.mtime,
        "is_dir": entry.is_dir,
    })
}

/// Print search hits to stdout in the requested format. Summaries and
/// warnings stay on stderr so machine formats parse cleanly. The
/// document formats stream row by row (bounded memory); only
/// `text`/`csv`/`json` exceptions below differ.
fn print_hits(
    hits: &[&FileEntry],
    format: OutputFormat,
    csv_delim: Option<char>,
) -> Result<(), String> {
    use supersearch_core::export;
    if csv_delim.is_some() && format != OutputFormat::Csv {
        return Err("--csv-delim only applies to --format csv".to_string());
    }
    // The document formats share one owned copy; the streaming arms
    // below borrow the originals.
    let owned: Vec<FileEntry> = hits.iter().map(|e| (*e).clone()).collect();
    // Stream document formats straight to stdout: no whole-document
    // `String` no matter the hit count.
    let stream = |fmt: export::ExportFormat| {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        export::write_format(&mut out, &owned, fmt, None)
            .map_err(|e| format!("write {format:?}: {e}"))
    };
    match format {
        OutputFormat::Html => stream(export::ExportFormat::Html),
        OutputFormat::Rtf => stream(export::ExportFormat::Rtf),
        OutputFormat::Xlsx => stream(export::ExportFormat::ExcelXml),
        OutputFormat::Text => {
            for e in hits {
                println!("{}", e.path.display());
            }
            Ok(())
        }
        OutputFormat::Csv => {
            // LF terminator like the core/GUI renderer: both
            // frontends emit byte-identical CSV.
            let mut writer = csv::WriterBuilder::new()
                .delimiter(csv_delim.unwrap_or(',') as u8)
                .terminator(csv::Terminator::Any(b'\n'))
                .from_writer(std::io::stdout());
            writer
                .write_record(["path", "size", "mtime", "is_dir"])
                .map_err(|e| format!("write csv: {e}"))?;
            for e in hits {
                writer
                    .write_record([
                        e.path.display().to_string(),
                        e.size.to_string(),
                        e.mtime.to_string(),
                        e.is_dir.to_string(),
                    ])
                    .map_err(|e| format!("write csv: {e}"))?;
            }
            writer.flush().map_err(|e| format!("write csv: {e}"))?;
            Ok(())
        }
        OutputFormat::Json => {
            let values: Vec<serde_json::Value> = hits.iter().map(|e| entry_to_json(e)).collect();
            serde_json::to_string_pretty(&values)
                .map(|text| println!("{text}"))
                .map_err(|e| format!("write json: {e}"))?;
            Ok(())
        }
    }
}

/// Bring `db` up to date for `roots` before an indexed search: one
/// daemon-equivalent maintenance pass per root group. Local Windows
/// volumes read delta ticks (full fill where the journal wrapped or no
/// cursor exists yet); everything else re-polls with a walk. Reports
/// what ran on stderr; any failure is an error, never a silent skip.
fn refresh_index_for_search(db: &Path, roots: &[PathBuf], skip_hidden: bool) -> Result<(), String> {
    let mut conn =
        rusqlite::Connection::open(db).map_err(|e| format!("open db {}: {e}", db.display()))?;
    init_db(&conn).map_err(|e| format!("init db: {e}"))?;
    let pb = progress_bar();
    let pb2 = pb.clone();
    let progress = move |n: u64| pb2.set_message(format!("{n} files indexed"));
    #[cfg(windows)]
    {
        use std::collections::BTreeMap;
        let mut by_volume: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        let mut nonlocal = Vec::new();
        for root in roots {
            match volume_for_root(root) {
                Some(v) => by_volume.entry(v).or_default().push(root.clone()),
                None => nonlocal.push(root.clone()),
            }
        }
        if !nonlocal.is_empty() {
            let walk_opts = ScanOptions {
                roots: nonlocal,
                follow_symlinks: false,
                skip_hidden,
            };
            let stats = scan_to_db(&mut conn, &walk_opts, Some(&progress))
                .map_err(|e| format!("refresh walk roots: {e}"))?;
            pb.finish_and_clear();
            eprintln!("refreshed walk roots ({} scanned)", stats.scanned);
        }
        for (volume, vroots) in &by_volume {
            let anchor = volume_root_path(volume)
                .ok_or_else(|| format!("refresh {volume}: no filesystem root"))?;
            match usn_tick(&conn, &anchor, volume, skip_hidden)
                .map_err(|e| format!("refresh {volume}: tick failed: {e}"))?
            {
                UsnTickOutcome::Current => eprintln!("refreshed {volume}: already current"),
                UsnTickOutcome::Applied {
                    upserted, removed, ..
                } => eprintln!("refreshed {volume}: +{upserted}/-{removed}"),
                UsnTickOutcome::NeedsFullScan { reason } => {
                    let fill_opts = ScanOptions {
                        roots: vroots.clone(),
                        follow_symlinks: false,
                        skip_hidden,
                    };
                    let stats = index_fill_mft(&mut conn, &fill_opts, Some(&progress))
                        .map_err(|e| format!("refresh {volume}: fill failed: {e}"))?;
                    pb.finish_and_clear();
                    eprintln!(
                        "refreshed {volume}: full fill ({} rows, {} tombstoned; {reason})",
                        stats.rows, stats.tombstoned
                    );
                }
                UsnTickOutcome::Unsupported { reason } => {
                    let fill_opts = ScanOptions {
                        roots: vroots.clone(),
                        follow_symlinks: false,
                        skip_hidden,
                    };
                    let stats = scan_to_db(&mut conn, &fill_opts, Some(&progress))
                        .map_err(|e| format!("refresh {volume}: walk failed: {e}"))?;
                    pb.finish_and_clear();
                    eprintln!(
                        "refreshed {volume}: walk poll ({} scanned; {reason})",
                        stats.scanned
                    );
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        let opts = ScanOptions {
            roots: roots.to_vec(),
            follow_symlinks: false,
            skip_hidden,
        };
        let stats =
            scan_to_db(&mut conn, &opts, Some(&progress)).map_err(|e| format!("refresh: {e}"))?;
        pb.finish_and_clear();
        eprintln!("refreshed ({} scanned)", stats.scanned);
    }
    Ok(())
}

fn run_search(mut args: SearchArgs, config_path: &Path) -> Result<(), String> {
    let mut config = load_config_for_run(config_path);
    // File groups merge into the extension allow-list before the query
    // is built; unknown names fail loudly instead of matching nothing.
    for name in &args.group {
        match config.resolve_file_group(name) {
            Some(extensions) => args.ext.extend(extensions),
            None => return Err(format!("unknown file group: {name}")),
        }
    }
    // Flags win; config fills the gaps so `search` works with no flags
    // right after a `scan` remembered its roots/db.
    let roots = if args.root.is_empty() {
        config.roots.clone()
    } else {
        args.root.clone()
    };
    // Missing roots fail loudly instead of silently scanning nothing
    // (or, on the MFT path, scanning a whole volume for zero hits).
    // Exotic-but-valid spellings (volume devices) may not resolve via
    // `exists`, so they only warn; ordinary missing paths are dropped,
    // and all-missing is an error.
    let (roots, missing): (Vec<PathBuf>, Vec<PathBuf>) =
        roots.into_iter().partition(|r| r.exists());
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(|r| r.display().to_string()).collect();
        eprintln!(
            "supersearch: warning: skipping missing root(s): {}",
            names.join(", ")
        );
    }
    if args.live && roots.is_empty() {
        return Err("pass at least one --root with --live".to_string());
    }
    // `--live` ignores every index, explicit or remembered; otherwise
    // flags win and the config fills the gaps.
    let db = if args.live {
        None
    } else {
        args.db.clone().or_else(|| config.db_path.clone())
    };
    if roots.is_empty() && db.is_none() {
        return Err(
            "pass at least one --root or --db (or run `scan` first to remember them)".to_string(),
        );
    }
    if args.fresh {
        let db_path = db.as_ref().ok_or("pass --db with --fresh".to_string())?;
        if roots.is_empty() {
            return Err("pass --root with --fresh".to_string());
        }
        refresh_index_for_search(db_path, &roots, !args.include_hidden)?;
    }
    // An invalid regex matches nothing by design; say so loudly instead
    // of silently printing an empty result.
    if let Some(pattern) = &args.regex {
        if !regex_is_valid(pattern) {
            eprintln!("supersearch: warning: invalid --regex {pattern:?}: matches nothing");
        }
    }
    let query = build_search_query(&args, &config.exclusions)?;

    // Indexed entries are filtered here; on-demand roots go through the
    // fastest path (MFT name table on elevated Windows, tuned walk
    // elsewhere) and arrive already filtered.
    let mut hits: Vec<FileEntry> = Vec::new();
    let mut scanned = 0u64;
    let mut sources: Vec<String> = Vec::new();
    if let Some(db_path) = &db {
        // Searching never creates an index: a typo'd path must error,
        // not report a confident "0 hits" (creation is `scan`'s job).
        if !db_path.exists() {
            return Err(format!(
                "no such index file {}: run `scan` first",
                db_path.display()
            ));
        }
        let db_entries =
            load_db(db_path).map_err(|e| format!("read db {}: {e}", db_path.display()))?;
        scanned += db_entries.len() as u64;
        hits.extend(search(&db_entries, &query).into_iter().cloned());
        sources.push(format!("index {}", db_path.display()));
    }
    if !roots.is_empty() {
        let options = ScanOptions {
            roots: roots.clone(),
            follow_symlinks: args.follow_symlinks,
            skip_hidden: !args.include_hidden,
        };
        let pb = progress_bar();
        let pb2 = pb.clone();
        let progress = move |n: u64| pb2.set_message(format!("{n} files scanned"));
        // Text output over a name-only query needs no size/mtime, so the
        // MFT path can skip per-file stats entirely; csv/json always do.
        let want_metadata = query.needs_metadata() || args.format != OutputFormat::Text;
        let outcome =
            supersearch_index::fast_search(&options, &query, want_metadata, Some(&progress));
        pb.finish_and_clear();
        scanned += outcome.scanned;
        if outcome.skipped > 0 || outcome.errors > 0 {
            eprintln!("({} skipped, {} errors)", outcome.skipped, outcome.errors);
        }
        hits.extend(outcome.entries);
        sources.push(outcome.note);
    }
    // Indexed + on-demand sources overlap: first occurrence wins so
    // `--limit` slices a de-duplicated list, not an arbitrary half.
    {
        let mut seen = std::collections::HashSet::new();
        hits.retain(|e| seen.insert(e.path.clone()));
    }

    let refs: Vec<&FileEntry> = hits.iter().collect();
    let shown = if args.limit == 0 {
        &refs[..]
    } else {
        &refs[..refs.len().min(args.limit)]
    };
    print_hits(shown, args.format, args.csv_delim)?;
    eprintln!(
        "{} hits ({} scanned via {})",
        hits.len(),
        scanned,
        sources.join(" + ")
    );
    config.record_usage(&roots, db.as_ref());
    config.push_recent(describe_search(&args));
    save_config_after_run(config_path, &config);
    Ok(())
}

fn run_config(args: ConfigArgs, config_path: &Path) -> Result<(), String> {
    match args.action {
        ConfigAction::Path => {
            println!("{}", config_path.display());
            Ok(())
        }
        ConfigAction::Show => {
            let config = load_config_for_run(config_path);
            println!("# {}", config_path.display());
            serde_json::to_string_pretty(&config)
                .map(|text| println!("{text}"))
                .map_err(|e| format!("serialize config: {e}"))?;
            Ok(())
        }
        ConfigAction::Reset => {
            let config = AppConfig::default();
            config
                .save_to(config_path)
                .map_err(|e| format!("reset config {}: {e}", config_path.display()))?;
            println!("reset {}", config_path.display());
            Ok(())
        }
    }
}

/// Open `path` with the platform default application (detached).
fn run_open(args: PathArgs) -> Result<(), String> {
    if !args.path.exists() {
        return Err(format!(
            "no such file or directory: {}",
            args.path.display()
        ));
    }
    #[cfg(target_os = "windows")]
    let spawn = std::process::Command::new("cmd")
        .args(["/C", "start", "", &args.path.display().to_string()])
        .spawn();
    #[cfg(target_os = "macos")]
    let spawn = std::process::Command::new("open").arg(&args.path).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let spawn = std::process::Command::new("xdg-open")
        .arg(&args.path)
        .spawn();
    spawn
        .map(|_| ())
        .map_err(|e| format!("open {}: {e}", args.path.display()))?;
    Ok(())
}

/// Reveal `path` in the system file manager (selecting files, opening
/// directories).
fn run_reveal(args: PathArgs) -> Result<(), String> {
    if !args.path.exists() {
        return Err(format!(
            "no such file or directory: {}",
            args.path.display()
        ));
    }
    let is_dir = args.path.is_dir();
    #[cfg(target_os = "windows")]
    let spawn = {
        // `explorer /select,<file>` highlights the file; for a
        // directory just open it.
        let absolute = args
            .path
            .canonicalize()
            .unwrap_or_else(|_| args.path.clone());
        if is_dir {
            std::process::Command::new("explorer")
                .arg(&absolute)
                .spawn()
        } else {
            std::process::Command::new("explorer")
                .arg(format!("/select,{}", absolute.display()))
                .spawn()
        }
    };
    // `open -R` selects the file; for a directory open it.
    #[cfg(target_os = "macos")]
    let spawn = if is_dir {
        std::process::Command::new("open").arg(&args.path).spawn()
    } else {
        std::process::Command::new("open")
            .args(["-R"])
            .arg(&args.path)
            .spawn()
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let spawn = {
        // No ubiquitous "select" verb on Linux: open the directory
        // itself, or the parent for a file.
        let target: PathBuf = if is_dir {
            args.path.clone()
        } else {
            args.path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        };
        std::process::Command::new("xdg-open").arg(&target).spawn()
    };
    spawn
        .map(|_| ())
        .map_err(|e| format!("reveal {}: {e}", args.path.display()))?;
    Ok(())
}

/// Copy the canonicalized `path` to the system clipboard and echo it to
/// stdout so the command composes in scripts.
fn run_copy_path(args: PathArgs) -> Result<(), String> {
    let absolute = args
        .path
        .canonicalize()
        .unwrap_or_else(|_| args.path.clone());
    let text = absolute.display().to_string();
    arboard::Clipboard::new()
        .and_then(|mut cb| cb.set_text(text.clone()))
        .map_err(|e| format!("copy {} to clipboard: {e}", args.path.display()))?;
    println!("{text}");
    Ok(())
}

/// Run the USN-maintained index daemon in the foreground: prints a
/// status line per tick until Ctrl-C (exit 130 via the shared handler).
fn run_daemon(args: DaemonArgs, config_path: &Path) -> Result<(), String> {
    let mut config = AppConfig::load_from(config_path).map_err(|e| e.to_string())?;
    let roots = if args.root.is_empty() {
        config.roots.clone()
    } else {
        args.root.clone()
    };
    if roots.is_empty() {
        return Err("no roots: pass --root or run scan first".to_string());
    }
    let db = args
        .db
        .clone()
        .or(config.db_path.clone())
        .ok_or("no index database: pass --db or run scan first")?;
    config.record_usage(&roots, Some(&db));
    config
        .save_to(config_path)
        .map_err(|e| format!("save config {}: {e}", config_path.display()))?;
    let interval = std::time::Duration::from_secs(args.interval_secs.max(1));
    let options = ScanOptions {
        roots,
        follow_symlinks: false,
        skip_hidden: true,
    };
    let handle = spawn_index_thread(db.clone(), options, interval);
    eprintln!(
        "indexing {} (every {}s, Ctrl-C to stop)",
        db.display(),
        interval.as_secs()
    );
    loop {
        std::thread::sleep(interval);
        let status = handle.status();
        eprintln!(
            "tick: last_indexed={} stale={}",
            status
                .last_indexed
                .map(|t| t
                    .elapsed()
                    .map(|d| format!("{}s ago", d.as_secs()))
                    .unwrap_or_else(|_| "clock-skew".to_string()))
                .unwrap_or_else(|| "never".to_string()),
            status.stale
        );
    }
}

/// Install a Ctrl-C handler so long scans/searches cancel promptly with
/// exit code 130 instead of running to completion. Config is only
/// persisted after a successful run, so a cancelled run also leaves no
/// half-remembered state behind.
fn install_cancel_handler() {
    let _ = ctrlc::set_handler(|| {
        eprintln!("\nsupersearch: cancelled (Ctrl-C)");
        // Exit directly: the handler runs on a signal thread while the
        // main thread may be blocked in a scan.
        std::process::exit(130);
    });
}

fn main() {
    install_cancel_handler();
    let cli = Cli::parse();
    let config_path = resolve_config_path(cli.config.as_deref());
    let result = match cli.command {
        Command::Scan(args) => run_scan(args, &config_path),
        Command::Search(args) => run_search(args, &config_path),
        Command::Open(args) => run_open(args),
        Command::Reveal(args) => run_reveal(args),
        Command::CopyPath(args) => run_copy_path(args),
        Command::Config(args) => run_config(args, &config_path),
        Command::Daemon(args) => run_daemon(args, &config_path),
    };
    if let Err(e) = result {
        eprintln!("supersearch: error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The new filter flags must parse and reach the search query:
    /// regression coverage for wiring `--glob` / `--ext` /
    /// `--ignore-case` through to [`Query`].
    #[test]
    fn search_flags_wire_new_filters_into_query() {
        let cli = Cli::try_parse_from([
            "supersearch",
            "search",
            "--root",
            "crates",
            "--glob",
            "*.rs",
            "--ext",
            "rs",
            "--ext",
            ".md",
            "--ignore-case",
        ])
        .expect("parse");
        let Command::Search(args) = cli.command else {
            panic!("expected search subcommand");
        };
        let query = build_search_query(&args, &["target".to_string()]).expect("build query");
        assert_eq!(query.glob.as_deref(), Some("*.rs"));
        assert_eq!(query.extensions, vec!["rs".to_string(), ".md".to_string()]);
        assert!(query.case_insensitive);
        // Flag exclusions merge with (never replace) the config ones.
        assert_eq!(query.exclude, vec!["target".to_string()]);
    }

    /// `--or` / `--not` parse repeatably and become single-substring
    /// subqueries — the same shape the GUI builds from its fields.
    #[test]
    fn search_or_not_flags_wire_into_query() {
        let cli = Cli::try_parse_from([
            "supersearch",
            "search",
            "--or",
            "report",
            "--or",
            "invoice",
            "--not",
            "draft",
        ])
        .expect("parse");
        let Command::Search(args) = cli.command else {
            panic!("expected search subcommand");
        };
        let query = build_search_query(&args, &[]).expect("build query");
        let ors: Vec<_> = query
            .or
            .iter()
            .filter_map(|q| q.name_substr.clone())
            .collect();
        let nots: Vec<_> = query
            .not
            .iter()
            .filter_map(|q| q.name_substr.clone())
            .collect();
        assert_eq!(ors, vec!["report".to_string(), "invoice".to_string()]);
        assert_eq!(nots, vec!["draft".to_string()]);
        assert!(describe_search(&args).contains("any:report"));
        assert!(describe_search(&args).contains("not:draft"));
    }

    #[test]
    fn search_live_and_fresh_flags_parse_and_conflict() {
        let cli = Cli::try_parse_from(["supersearch", "search", "--root", r"C:\", "--live"])
            .expect("parse");
        let Command::Search(args) = cli.command else {
            panic!("expected search subcommand");
        };
        assert!(args.live);
        assert!(!args.fresh);
        let cli = Cli::try_parse_from([
            "supersearch",
            "search",
            "--root",
            r"C:\",
            "--db",
            "idx.db",
            "--fresh",
        ])
        .expect("parse");
        let Command::Search(args) = cli.command else {
            panic!("expected search subcommand");
        };
        assert!(args.fresh);
        assert!(!args.live);
        assert_eq!(describe_search(&args), "mode:fresh");
        // The two query-path switches are mutually exclusive.
        assert!(Cli::try_parse_from(["supersearch", "search", "--live", "--fresh"]).is_err());
    }

    #[test]
    fn search_group_flag_parses() {
        let cli = Cli::try_parse_from([
            "supersearch",
            "search",
            "--group",
            "Documents",
            "--group",
            "Code",
        ])
        .expect("parse");
        let Command::Search(args) = cli.command else {
            panic!("expected search subcommand");
        };
        assert_eq!(
            args.group,
            vec!["Documents".to_string(), "Code".to_string()]
        );
        assert!(describe_search(&args).contains("group:Documents"));
    }

    #[test]
    fn csv_delim_parses_names_chars_and_rejects_junk() {
        assert_eq!(parse_csv_delim("comma"), Ok(','));
        assert_eq!(parse_csv_delim("semicolon"), Ok(';'));
        assert_eq!(parse_csv_delim("tab"), Ok('\t'));
        assert_eq!(parse_csv_delim("pipe"), Ok('|'));
        assert_eq!(parse_csv_delim(";"), Ok(';'));
        assert!(parse_csv_delim(";;").is_err());
        assert!(parse_csv_delim("").is_err());
        assert!(parse_csv_delim("\"").is_err());
        assert!(parse_csv_delim("é").is_err());
        let cli = Cli::try_parse_from([
            "supersearch",
            "search",
            "--format",
            "csv",
            "--csv-delim",
            "semicolon",
        ])
        .expect("parse");
        let Command::Search(args) = cli.command else {
            panic!("expected search subcommand");
        };
        assert_eq!(args.csv_delim, Some(';'));
        // A delimiter with any other format is a usage error, not a
        // silently ignored flag.
        let err = print_hits(&[], OutputFormat::Json, Some(';')).unwrap_err();
        assert!(err.contains("--csv-delim"), "was: {err}");
        assert!(print_hits(&[], OutputFormat::Csv, Some(';')).is_ok());
    }
}

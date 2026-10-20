//! Render hit lists for export: plain text, CSV, HTML tables, RTF,
//! and Excel-readable XML Spreadsheet 2003. No extra dependencies, so
//! both the CLI `--format` arms and the GUI export buttons share one
//! implementation.
//!
//! Every format is a `write_*` streaming row writer (bounded memory no
//! matter the hit count) plus a `to_*` convenience that collects the
//! same bytes into a `String`.

use std::io::{Result as IoResult, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use super::FileEntry;

/// Progress/cancel hook polled by the streaming `write_*` exporters so
/// huge hit lists report rows-written and abort promptly instead of
/// freezing the caller. The row callback fires every 1024 rows plus the
/// final row (not per row, so channel/closure traffic stays flat); the
/// cancel flag is checked on every row and aborts the write with an
/// [`std::io::ErrorKind::Interrupted`] error.
pub struct ExportHook<'a> {
    /// Called with rows written so far.
    pub on_rows: &'a dyn Fn(usize),
    /// Writers abort when this flips.
    pub cancel: &'a AtomicBool,
}

/// One hook tick for row `rows` (1-based) of `total`: report progress at
/// stride boundaries and fail when cancellation was requested.
fn hook_row(hook: Option<&ExportHook>, rows: usize, total: usize) -> IoResult<()> {
    if let Some(h) = hook {
        if rows == total || rows % 1024 == 0 {
            (h.on_rows)(rows);
        }
        if h.cancel.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "export cancelled",
            ));
        }
    }
    Ok(())
}

/// Selectable export rendering, shared by the CLI `--format` arms and
/// the GUI export buttons so both frontends produce byte-identical
/// files. `Csv` carries the field delimiter (`,` by default; `;`, tab,
/// and `|` suit locales where the comma is a decimal separator).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Txt,
    Csv(char),
    Html,
    Rtf,
    ExcelXml,
}

/// Stream `entries` in `format` to `w`, honoring `hook` for progress
/// and cancellation. Bounded memory no matter the hit count.
pub fn write_format(
    w: &mut dyn Write,
    entries: &[FileEntry],
    format: ExportFormat,
    hook: Option<&ExportHook>,
) -> IoResult<()> {
    match format {
        ExportFormat::Txt => write_txt(w, entries, hook),
        ExportFormat::Csv(delim) => write_csv_delim(w, entries, delim, hook),
        ExportFormat::Html => write_html(w, entries, hook),
        ExportFormat::Rtf => write_rtf(w, entries, hook),
        ExportFormat::ExcelXml => write_excel_xml(w, entries, hook),
    }
}

/// Collect a streaming writer's bytes. `Vec` writes are infallible, so
/// this never loses data; the lossy fallback only guards the
/// theoretically-impossible invalid UTF-8.
fn collect(write: impl FnOnce(&mut Vec<u8>) -> IoResult<()>) -> String {
    let mut buf = Vec::new();
    let _ = write(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// One hit path per line (matches the CLI `text` rendering).
pub fn write_txt(
    w: &mut dyn Write,
    entries: &[FileEntry],
    hook: Option<&ExportHook>,
) -> IoResult<()> {
    let total = entries.len();
    for (i, e) in entries.iter().enumerate() {
        writeln!(w, "{}", e.path.display())?;
        hook_row(hook, i + 1, total)?;
    }
    Ok(())
}

/// One hit path per line (matches the CLI `text` rendering).
pub fn to_txt(entries: &[FileEntry]) -> String {
    collect(|w| write_txt(w, entries, None))
}

/// Quote a CSV field when it carries the delimiter, `"`, or a line
/// break (`\r` included: a bare carriage return would corrupt the row
/// just like `\n` does).
fn csv_field_into(out: &mut String, s: &str, delim: char) {
    if s.contains(delim) || s.contains('"') || s.contains('\n') || s.contains('\r') {
        out.push('"');
        out.push_str(&s.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(s);
    }
}

/// `path,size,mtime,is_dir` rows (delimiter-separated) with a header line.
pub fn write_csv_delim(
    w: &mut dyn Write,
    entries: &[FileEntry],
    delim: char,
    hook: Option<&ExportHook>,
) -> IoResult<()> {
    writeln!(w, "path{delim}size{delim}mtime{delim}is_dir")?;
    let total = entries.len();
    let mut line = String::new();
    for (i, e) in entries.iter().enumerate() {
        line.clear();
        csv_field_into(&mut line, &e.path.display().to_string(), delim);
        line.push(delim);
        line.push_str(&e.size.to_string());
        line.push(delim);
        line.push_str(&e.mtime.to_string());
        line.push(delim);
        line.push_str(if e.is_dir { "true" } else { "false" });
        writeln!(w, "{line}")?;
        hook_row(hook, i + 1, total)?;
    }
    Ok(())
}

/// `path,size,mtime,is_dir` rows with a header line (comma delimiter).
pub fn write_csv(
    w: &mut dyn Write,
    entries: &[FileEntry],
    hook: Option<&ExportHook>,
) -> IoResult<()> {
    write_csv_delim(w, entries, ',', hook)
}

/// `path,size,mtime,is_dir` rows with a header line (comma delimiter).
pub fn to_csv(entries: &[FileEntry]) -> String {
    collect(|w| write_csv(w, entries, None))
}

/// Delimited rows with a header line, for `--csv-delim` / the GUI
/// delimiter picker.
pub fn to_csv_delim(entries: &[FileEntry], delim: char) -> String {
    collect(|w| write_csv_delim(w, entries, delim, None))
}

/// Minimal `&`, `<`, `>`, `"` escaping shared by the HTML and XML arms.
fn esc_markup_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

/// Standalone HTML document with one table row per hit.
pub fn write_html(
    w: &mut dyn Write,
    entries: &[FileEntry],
    hook: Option<&ExportHook>,
) -> IoResult<()> {
    writeln!(
        w,
        "<!DOCTYPE html>\n<html>\n<head><meta charset=\"utf-8\">\
         <title>SuperSearch results</title></head>\n<body>\n\
         <table>\n<thead><tr><th>path</th><th>size</th>\
         <th>mtime</th><th>is_dir</th></tr></thead>\n<tbody>"
    )?;
    let total = entries.len();
    let mut cell = String::new();
    for (i, e) in entries.iter().enumerate() {
        cell.clear();
        esc_markup_into(&mut cell, &e.path.display().to_string());
        writeln!(
            w,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            cell,
            e.size,
            e.mtime,
            if e.is_dir { "true" } else { "false" }
        )?;
        hook_row(hook, i + 1, total)?;
    }
    writeln!(w, "</tbody>\n</table>\n</body>\n</html>")
}

/// Standalone HTML document with one table row per hit.
pub fn to_html(entries: &[FileEntry]) -> String {
    collect(|w| write_html(w, entries, None))
}

/// Escape `\`, `{`, `}` for RTF literals; all other text passes through.
fn esc_rtf_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '\n' => out.push_str("\\par "),
            _ => out.push(c),
        }
    }
}

/// RTF document with one paragraph per hit (tab-separated columns).
/// Opens in WordPad/Word; kept paragraph-flat because RTF tables add
/// far more syntax than value here.
pub fn write_rtf(
    w: &mut dyn Write,
    entries: &[FileEntry],
    hook: Option<&ExportHook>,
) -> IoResult<()> {
    writeln!(w, "{{\\rtf1\\ansi\n{{\\b SuperSearch results\\b}}\\par")?;
    let total = entries.len();
    let mut path = String::new();
    for (i, e) in entries.iter().enumerate() {
        path.clear();
        esc_rtf_into(&mut path, &e.path.display().to_string());
        writeln!(
            w,
            "{}\\tab {}\\tab {}\\tab {}\\par",
            path,
            e.size,
            e.mtime,
            if e.is_dir { "true" } else { "false" }
        )?;
        hook_row(hook, i + 1, total)?;
    }
    writeln!(w, "}}")
}

/// RTF document with one paragraph per hit (tab-separated columns).
/// Opens in WordPad/Word; kept paragraph-flat because RTF tables add
/// far more syntax than value here.
pub fn to_rtf(entries: &[FileEntry]) -> String {
    collect(|w| write_rtf(w, entries, None))
}

/// XML Spreadsheet 2003 workbook: Excel opens it as a real worksheet
/// (string cells for paths, number cells for size/mtime).
pub fn write_excel_xml(
    w: &mut dyn Write,
    entries: &[FileEntry],
    hook: Option<&ExportHook>,
) -> IoResult<()> {
    writeln!(
        w,
        "<?xml version=\"1.0\"?>\n<Workbook \
         xmlns=\"urn:schemas-microsoft-com:office:spreadsheet\" \
         xmlns:ss=\"urn:schemas-microsoft-com:office:spreadsheet\">\n\
         <Worksheet ss:Name=\"Results\"><Table>\n\
         <Row><Cell><Data ss:Type=\"String\">path</Data></Cell>\
         <Cell><Data ss:Type=\"String\">size</Data></Cell>\
         <Cell><Data ss:Type=\"String\">mtime</Data></Cell>\
         <Cell><Data ss:Type=\"String\">is_dir</Data></Cell></Row>"
    )?;
    let total = entries.len();
    let mut cell = String::new();
    for (i, e) in entries.iter().enumerate() {
        cell.clear();
        esc_markup_into(&mut cell, &e.path.display().to_string());
        writeln!(
            w,
            "<Row><Cell><Data ss:Type=\"String\">{}</Data></Cell>\
             <Cell><Data ss:Type=\"Number\">{}</Data></Cell>\
             <Cell><Data ss:Type=\"Number\">{}</Data></Cell>\
             <Cell><Data ss:Type=\"String\">{}</Data></Cell></Row>",
            cell,
            e.size,
            e.mtime,
            if e.is_dir { "true" } else { "false" }
        )?;
        hook_row(hook, i + 1, total)?;
    }
    writeln!(w, "</Table></Worksheet>\n</Workbook>")
}

/// XML Spreadsheet 2003 workbook: Excel opens it as a real worksheet
/// (string cells for paths, number cells for size/mtime).
pub fn to_excel_xml(entries: &[FileEntry]) -> String {
    collect(|w| write_excel_xml(w, entries, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample() -> Vec<FileEntry> {
        vec![
            FileEntry::new(PathBuf::from(r"C:\a<b>&.txt"), 41, 1700000000, false),
            FileEntry::new(PathBuf::from(r"C:\d"), 0, 0, true),
        ]
    }

    #[test]
    fn txt_lists_one_path_per_line() {
        assert_eq!(to_txt(&sample()), "C:\\a<b>&.txt\nC:\\d\n");
        assert_eq!(to_txt(&[]), "");
    }

    #[test]
    fn csv_quotes_tricky_fields() {
        let rows = vec![FileEntry::new(PathBuf::from("C:\\a,b\"c.txt"), 7, 9, false)];
        assert_eq!(
            to_csv(&rows),
            "path,size,mtime,is_dir\n\"C:\\a,b\"\"c.txt\",7,9,false\n"
        );
        assert!(to_csv(&sample()).starts_with("path,size,mtime,is_dir\n"));
    }

    #[test]
    fn html_escapes_markup_and_closes_table() {
        let html = to_html(&sample());
        assert!(html.contains("<table>"));
        assert!(html.contains("C:\\a&lt;b&gt;&amp;.txt"));
        assert!(html.contains("</table>"));
        assert!(!html.contains("<b>&"));
    }

    #[test]
    fn rtf_escapes_braces_and_lists_rows() {
        let rtf = to_rtf(&sample());
        assert!(rtf.starts_with("{\\rtf1"));
        // The path backslash is RTF-escaped (doubled), like every
        // other literal backslash.
        assert!(
            rtf.contains("C:\\\\a<b>&.txt\\tab 41\\tab"),
            "RTF was: {rtf:?}"
        );
        assert!(rtf.ends_with("}\n"));
    }

    #[test]
    fn excel_xml_marks_types_and_escapes() {
        let xml = to_excel_xml(&sample());
        assert!(xml.contains("<Workbook"));
        assert!(xml.contains("ss:Type=\"Number\">41<"));
        assert!(xml.contains("C:\\a&lt;b&gt;&amp;.txt"));
        assert!(xml.contains("</Workbook>"));
    }

    #[test]
    fn csv_delim_switches_separator_and_quoting() {
        let rows = vec![FileEntry::new(PathBuf::from("C:\\a;b,c.txt"), 7, 9, false)];
        // Semicolon mode quotes `;` but leaves a bare comma alone.
        assert_eq!(
            to_csv_delim(&rows, ';'),
            "path;size;mtime;is_dir\n\"C:\\a;b,c.txt\";7;9;false\n"
        );
        // Comma mode keeps quoting `,` but not `;`.
        assert_eq!(
            to_csv(&rows),
            "path,size,mtime,is_dir\n\"C:\\a;b,c.txt\",7,9,false\n"
        );
        // Tab header joins with tabs.
        assert!(to_csv_delim(&rows, '\t').starts_with("path\tsize\tmtime\tis_dir\n"));
        // A carriage return quotes like a newline does.
        let cr = vec![FileEntry::new(PathBuf::from("C:\\a\rb.txt"), 1, 2, false)];
        assert_eq!(
            to_csv(&cr),
            "path,size,mtime,is_dir\n\"C:\\a\rb.txt\",1,2,false\n"
        );
    }

    #[test]
    fn hook_reports_final_row_count() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Mutex;
        let seen = Mutex::new(Vec::new());
        let cancel = AtomicBool::new(false);
        let hook = ExportHook {
            on_rows: &|n| seen.lock().unwrap().push(n),
            cancel: &cancel,
        };
        let mut buf = Vec::new();
        write_txt(&mut buf, &sample(), Some(&hook)).unwrap();
        // Two rows: only the final-row tick fires below the stride.
        assert_eq!(*seen.lock().unwrap(), vec![2]);
    }

    /// 200k rows stream with throttled, monotonic progress ending on
    /// the total: the determinate-bar data source under load.
    #[test]
    fn hook_progress_covers_large_exports() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Mutex;
        let rows: Vec<FileEntry> = (0..200_000)
            .map(|i| {
                FileEntry::new(
                    PathBuf::from(format!("C:\\dir\\file{i:06}.txt")),
                    i,
                    1700000000,
                    false,
                )
            })
            .collect();
        let seen = Mutex::new(Vec::new());
        let cancel = AtomicBool::new(false);
        let hook = ExportHook {
            on_rows: &|n| seen.lock().unwrap().push(n),
            cancel: &cancel,
        };
        let mut buf = Vec::new();
        write_csv(&mut buf, &rows, Some(&hook)).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(*seen.last().unwrap(), rows.len());
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "progress regressed");
        // Header + one line per row, nothing buffered whole.
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), rows.len() + 1);
    }

    #[test]
    fn hook_cancel_aborts_with_interrupted() {
        use std::sync::atomic::AtomicBool;
        let cancel = AtomicBool::new(true);
        let hook = ExportHook {
            on_rows: &|_| {},
            cancel: &cancel,
        };
        let mut buf = Vec::new();
        let err = write_csv(&mut buf, &sample(), Some(&hook)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
    }

    /// Cancelling after the first stride tick aborts mid-stream: the
    /// writer stops with `Interrupted` and leaves a strict prefix of
    /// the full export, never the whole file.
    #[test]
    fn hook_cancel_mid_stream_aborts_with_interrupted() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let rows: Vec<FileEntry> = (0..3000u64)
            .map(|i| {
                FileEntry::new(
                    PathBuf::from(format!("C:\\rows\\f{i:04}.txt")),
                    i,
                    1700000000,
                    false,
                )
            })
            .collect();
        let cancel = AtomicBool::new(false);
        let hook = ExportHook {
            on_rows: &|n| {
                if n >= 1024 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            cancel: &cancel,
        };
        let mut buf = Vec::new();
        let err = write_csv(&mut buf, &rows, Some(&hook)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
        let lines = buf.iter().filter(|&&b| b == b'\n').count();
        assert!(
            lines > 1 && lines < rows.len() + 1,
            "expected a partial export, got {lines} lines"
        );
    }
}

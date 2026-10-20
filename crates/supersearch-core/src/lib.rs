use std::path::PathBuf;

/// Hit-list renderers shared by the CLI `--format` arms and the GUI
/// export buttons (plain text, HTML, RTF, Excel XML).
pub mod export;

#[cfg(windows)]
pub mod shell;

/// A single indexed filesystem entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
}

impl FileEntry {
    pub fn new(path: PathBuf, size: u64, mtime: i64, is_dir: bool) -> Self {
        Self {
            path,
            size,
            mtime,
            is_dir,
        }
    }
}

/// Windows-only build step: stamp the release/debug exe with the
/// SuperSearch icon and version metadata (no external tools —
/// `winres` writes the `.res` itself). Non-Windows builds skip it.
#[cfg(windows)]
fn main() {
    let mut res = winres::WindowsResource::new();
    res.set_icon("assets/supersearch.ico");
    res.set("FileDescription", "SuperSearch fast file finder");
    res.set("ProductName", "SuperSearch");
    res.set("OriginalFilename", "supersearch-gui.exe");
    if let Err(e) = res.compile() {
        eprintln!("winres failed (exe will have no icon): {e}");
    }
}

#[cfg(not(windows))]
fn main() {}

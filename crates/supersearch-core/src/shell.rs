//! Windows shell actions shared by the desktop and terminal interfaces.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows_sys::Win32::{
    System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    },
    UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_INVOKEIDLIST, SEE_MASK_NOASYNC,
        SHELLEXECUTEINFOW,
    },
};

/// Pass the filename directly to the shell, without a command interpreter.
pub fn open_path(path: &Path) -> Result<(), String> {
    invoke(path, "open", false)
}

/// Properties is a context-menu extension verb, requiring INVOKEIDLIST.
pub fn show_file_properties(path: &Path) -> Result<(), String> {
    invoke(path, "properties", true)
}

fn invoke(path: &Path, verb: &str, context_menu: bool) -> Result<(), String> {
    let file: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let operation: Vec<u16> = verb.encode_utf16().chain(Some(0)).collect();
    // Balance successful initialization, including S_FALSE. A caller
    // already using another apartment keeps its existing COM setup.
    let initialized = unsafe {
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    } >= 0;
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC
            | SEE_MASK_FLAG_NO_UI
            | if context_menu {
                SEE_MASK_INVOKEIDLIST
            } else {
                0
            },
        lpVerb: operation.as_ptr(),
        lpFile: file.as_ptr(),
        nShow: 5,
        ..Default::default()
    };
    let success = unsafe { ShellExecuteExW(&mut info) } != 0;
    let error = std::io::Error::last_os_error();
    if initialized {
        unsafe { CoUninitialize() };
    }
    if success {
        Ok(())
    } else {
        Err(format!("{verb} failed for {}: {error}", path.display()))
    }
}

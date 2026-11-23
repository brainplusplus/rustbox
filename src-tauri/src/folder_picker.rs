//! Native "pick a folder" dialog for the data-root setting.
//!
//! A path field alone is a bad control here: the value is a full Windows path
//! the user is expected to type correctly, and a typo does not fail loudly - it
//! creates a new empty folder and the workspace looks reset. The shell's own
//! picker gives back a path that already exists.
//!
//! `SHBrowseForFolderW` is used rather than the newer `IFileOpenDialog` because
//! it needs no COM interfaces: one struct, one call, one buffer. It does still
//! want an apartment-threaded COM initialisation for its tree view, which is why
//! this runs on its own thread rather than in the Tauri command handler.
//!
//! Other platforms return an explanatory error: the settings page falls back to
//! the text field there, which is honest about what exists.

/// Ask the user for a folder. `None` means they cancelled.
#[cfg(windows)]
pub fn pick(start_in: Option<&str>) -> Result<Option<String>, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::UI::Shell::{
        SHBrowseForFolderW, SHGetPathFromIDListW, BIF_EDITBOX, BIF_NEWDIALOGSTYLE,
        BIF_RETURNONLYFSDIRS, BROWSEINFOW,
    };

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let title = wide("Choose a folder for RustBox");
    let _ = start_in;
    // The shell writes the display name here; a null pointer is not accepted.
    let mut display = [0u16; 260];

    let bi = BROWSEINFOW {        hwndOwner: std::ptr::null_mut(),
        pidlRoot: std::ptr::null_mut(),
        pszDisplayName: display.as_mut_ptr(),
        lpszTitle: title.as_ptr(),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX,
        // No callback: the only message worth handling is `BFFM_INITIALIZED`, and
        // preselecting a folder needs `BFFM_SETSELECTIONW` through `SendMessage`,
        // which this crate does not link. Starting at the shell default is better
        // than a half-wired callback.
        lpfn: None,
        lParam: 0,
        iImage: 0,
    };

    let pidl = unsafe {
        // A dialog needs an apartment; S_FALSE (already initialised) is fine and
        // an explicit uninitialise is wrong here because other code may own it.
        let hr = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        if hr < 0 {
            return Err(format!("could not start the folder picker (0x{hr:08x})"));
        }
        SHBrowseForFolderW(&bi)
    };
    if pidl.is_null() {
        // Cancelling is not an error: the user closed the dialog on purpose.
        return Ok(None);
    }

    let mut buf = [0u16; 1024];
    let ok = unsafe { SHGetPathFromIDListW(pidl, buf.as_mut_ptr()) };
    unsafe { CoTaskMemFree(pidl as *const _) };
    if ok == 0 {
        return Err("the folder you chose has no filesystem path (a virtual folder such as This PC cannot hold the data)".to_string());
    }
    let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    let path = String::from_utf16_lossy(&buf[..len]);
    if path.is_empty() {
        return Ok(None);
    }
    Ok(Some(path))
}

#[cfg(not(windows))]
pub fn pick(_start_in: Option<&str>) -> Result<Option<String>, String> {
    Err(
        "There is no system folder picker on this platform yet — type the folder path instead."
            .to_string(),
    )
}

//! Native clipboard write.
//!
//! The page can put text on the clipboard through `navigator.clipboard`, and on a
//! focused window it does. WebView2, however, rejects that call with
//! `NotAllowedError: Document is not focused` whenever the webview's document does
//! not hold focus — and a Copy button can be clicked in exactly that state. The
//! click is delivered, the promise rejects, and the user is told the copy failed
//! even though the button was pressed correctly.
//!
//! Going through the Win32 clipboard API removes that dependency: the write is a
//! plain API call with no focus or permission precondition, so a copy either
//! works or fails for a reason worth reporting.

/// Put `text` on the clipboard as `CF_UNICODETEXT`.
///
/// Errors are worded for the user, because the only caller surfaces them.
#[cfg(windows)]
pub fn write_text(text: &str) -> Result<(), String> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    // A clipboard string is NUL-terminated UTF-16, so the terminator is part of
    // the allocation. An interior NUL would truncate the paste in every consumer,
    // so it is refused here rather than silently cut short.
    if text.contains('\0') {
        return Err("The text contains a NUL character.".to_string());
    }
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * std::mem::size_of::<u16>();

    // The clipboard is one shared resource: another process can hold it open for
    // a few milliseconds (clipboard history, a password manager). That is
    // transient, not a failure, so retry briefly instead of reporting an error
    // the user would have to re-trigger by hand.
    let mut opened = false;
    for _ in 0..10 {
        // SAFETY: a null owner is allowed; no window handle is needed to own the
        // clipboard.
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !opened {
        return Err("The clipboard is busy. Try again.".to_string());
    }

    // Everything below runs with the clipboard open, and every path out of it has
    // to close it: a clipboard left locked stays locked for the rest of the
    // session and the next copy fails too.
    //
    // SAFETY: the clipboard is open and owned by this thread, so each call below
    // is a valid clipboard operation; `wide` outlives the copy into the block.
    let result = unsafe {
        if EmptyClipboard() == 0 {
            Err("The clipboard refused the copy.".to_string())
        } else {
            // GMEM_MOVEABLE is required: the system takes ownership of the block
            // and frees it later, which a fixed block cannot survive.
            let handle = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if handle.is_null() {
                Err("Not enough memory to copy.".to_string())
            } else {
                let dest = GlobalLock(handle);
                if dest.is_null() {
                    Err("Not enough memory to copy.".to_string())
                } else {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), dest as *mut u16, wide.len());
                    GlobalUnlock(handle);
                    // On success the clipboard owns `handle`; only a failed
                    // hand-off would leave it ours to free.
                    if SetClipboardData(CF_UNICODETEXT as u32, handle).is_null() {
                        Err("The clipboard refused the copy.".to_string())
                    } else {
                        Ok(())
                    }
                }
            }
        }
    };

    // SAFETY: opened above, and no clipboard call is made after this.
    unsafe { CloseClipboard() };
    result
}

/// Non-Windows builds have no clipboard backend yet.
#[cfg(not(windows))]
pub fn write_text(_text: &str) -> Result<(), String> {
    Err("Copying is only implemented on Windows.".to_string())
}

#[cfg(test)]
mod tests {
    /// The terminator is part of the allocation, so a 5-character string needs 6
    /// UTF-16 units, not 5.
    #[test]
    fn the_terminator_is_included() {
        let mut wide: Vec<u16> = "abcde".encode_utf16().collect();
        wide.push(0);
        assert_eq!(wide.len(), 6);
        assert_eq!(wide.len() * std::mem::size_of::<u16>(), 12);
    }

    /// An interior NUL would truncate the paste, so it is rejected rather than
    /// silently cut short.
    #[test]
    fn interior_nul_is_detected() {
        assert!("a\0b".contains('\0'));
        assert!(!"combo/fast".contains('\0'));
    }

    /// Newlines survive: ids are copied one per line, and pasting them as
    /// separate lines is the whole point.
    #[test]
    fn newlines_survive() {
        let wide: Vec<u16> = "risun/a\ncombo/fast".encode_utf16().collect();
        assert_eq!(wide.iter().filter(|u| **u == 0x0a).count(), 1);
    }

    /// Non-BMP characters take two UTF-16 units, so the byte count cannot be
    /// derived from the string's length in bytes.
    #[test]
    fn astral_characters_take_two_units() {
        assert_eq!("a🎉".encode_utf16().count(), 3);
        assert_eq!("a🎉".len(), 5);
    }

    /// The whole mechanism, against the real system clipboard.
    ///
    /// The unit tests above check the buffer arithmetic, but none of them proves
    /// that Win32 accepts the handle and that a reader gets the same text back —
    /// which is the only claim that matters. The user's clipboard is saved and
    /// restored around the check so running the suite does not eat it.
    #[cfg(windows)]
    #[test]
    fn writes_and_reads_back() {
        use windows_sys::Win32::System::DataExchange::{
            CloseClipboard, GetClipboardData, OpenClipboard,
        };
        use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
        use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

        fn read_back() -> Option<String> {
            // SAFETY: balanced by CloseClipboard below; the returned handle is
            // owned by the clipboard and is only read, never freed.
            unsafe {
                if OpenClipboard(std::ptr::null_mut()) == 0 {
                    return None;
                }
                let handle = GetClipboardData(CF_UNICODETEXT as u32);
                let text = if handle.is_null() {
                    None
                } else {
                    let ptr = GlobalLock(handle) as *const u16;
                    if ptr.is_null() {
                        None
                    } else {
                        let mut len = 0usize;
                        while *ptr.add(len) != 0 {
                            len += 1;
                        }
                        let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                        GlobalUnlock(handle);
                        Some(s)
                    }
                };
                CloseClipboard();
                text
            }
        }

        // A machine whose clipboard is held open by another process would make
        // this flaky, and that is not what is under test.
        let original = read_back();
        let sentinel = "rustbox clipboard test ✓\ncombo/fast";

        super::write_text(sentinel).expect("write_text should succeed");
        assert_eq!(read_back().as_deref(), Some(sentinel));

        // Put back whatever was there before, so a test run is not destructive.
        if let Some(prev) = original {
            let _ = super::write_text(&prev);
        }
    }
}

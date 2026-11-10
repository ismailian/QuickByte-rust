//! Port of `src/QuickByte.UI/Controls/DarkMode.cs` (title bars; egui draws every
//! control itself, so the WinForms control theming has no equivalent).
//!
//! A dark window under a white title bar reads as broken rather than dark, and
//! the title bar is drawn by Windows, so only Windows can be asked to darken it.
//! Everything here is cosmetic, so every call is best-effort — a Windows that
//! does not understand the request simply keeps its light chrome.

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::Graphics::Dwm::{DWMWINDOWATTRIBUTE, DwmSetWindowAttribute};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible};
use windows::core::BOOL;

/// DWMWA_USE_IMMERSIVE_DARK_MODE; builds of Windows 10 before 20H1 used 19 for the same thing.
const IMMERSIVE_DARK_MODE: i32 = 20;
const IMMERSIVE_DARK_MODE_BEFORE_20H1: i32 = 19;

/// `DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)`: a dark (or light) title bar.
pub fn use_dark_title_bar(hwnd: isize, dark: bool) {
    if hwnd == 0 {
        return;
    }
    let value: i32 = dark as i32;
    let window = HWND(hwnd as _);
    let set = |attribute: i32| {
        // SAFETY: the value pointer and size describe one i32 for the duration of the call.
        unsafe {
            DwmSetWindowAttribute(window, DWMWINDOWATTRIBUTE(attribute), (&value as *const i32).cast(), size_of::<i32>() as u32)
        }
    };
    if set(IMMERSIVE_DARK_MODE).is_err() {
        let _ = set(IMMERSIVE_DARK_MODE_BEFORE_20H1);
    }
}

/// The visible top-level windows of this process whose title is exactly `title`
/// (how the UI finds the HWND of an egui viewport).
pub fn find_own_windows(title: &str) -> Vec<isize> {
    struct Search {
        title: Vec<u16>,
        process: u32,
        found: Vec<isize>,
    }

    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the &mut Search passed to EnumWindows below, alive for the enumeration.
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut process = 0u32;
        // SAFETY: plain queries on a window handle the system just handed us.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            if process == search.process && IsWindowVisible(hwnd).as_bool() {
                let mut buffer = vec![0u16; search.title.len() + 2];
                let n = GetWindowTextW(hwnd, &mut buffer).max(0) as usize;
                if buffer[..n] == search.title[..] {
                    search.found.push(hwnd.0 as isize);
                }
            }
        }
        BOOL(1)
    }

    let mut search = Search { title: title.encode_utf16().collect(), process: std::process::id(), found: Vec::new() };
    // SAFETY: the callback only touches `search`, which outlives the call.
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut search as *mut Search as isize));
    }
    search.found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_nobody_has_finds_nothing() {
        assert!(find_own_windows(&format!("QuickByte test {}", uuid::Uuid::new_v4())).is_empty());
    }

    #[test]
    fn a_null_window_is_left_alone() {
        use_dark_title_bar(0, true);
    }
}

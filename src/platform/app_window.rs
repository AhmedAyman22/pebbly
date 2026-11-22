//! Bringing an app's own window to the front from one of its cards (a media player, Claude Code's
//! terminal). The click was on Pebbly, so Windows lets the foreground be handed on.

use windows::core::{BOOL, GUID, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, PROPERTYKEY};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowLongW, GetWindowTextW, IsIconic, IsWindow, IsWindowVisible, SetForegroundWindow, ShowWindow,
    GWL_EXSTYLE, GW_OWNER, SW_RESTORE, SW_SHOWNORMAL, WS_EX_TOOLWINDOW,
};

use super::fullscreen::exe_of;

/// `PKEY_AppUserModel_ID`: the app id a window (or its process) told the shell.
const PKEY_APP_USER_MODEL_ID: PROPERTYKEY =
    PROPERTYKEY { fmtid: GUID::from_u128(0x9F4C2855_9F79_4B39_A8D0_E1D42DE1D5F3), pid: 5 };

/// `hwnd` comes up, restored if minimised. False when the window is gone.
pub fn bring_to_front(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return false;
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd).as_bool()
    }
}

/// The window of the app that reported media as `app_id` (the media session's source: an exe
/// name like "Spotify.exe", or an app id like "Chrome" or a Store app's "Family!App") comes up.
/// With several, the one whose title names `title` wins (a browser tab), else the topmost one.
/// With none on screen (Spotify closed to the tray, a suspended Store app), the app is asked to
/// show itself. False when nothing could be done.
pub fn focus_app(app_id: &str, title: &str) -> bool {
    let windows = app_windows();
    let found = pick(&windows, app_id, title);
    if let Some(hwnd) = found {
        return bring_to_front(hwnd);
    }
    // Store apps (and Desktop Bridge ones) are activated through their app id; Spotify through
    // its own protocol, which shows the running instance's window.
    let target = if app_id.contains('!') {
        format!("shell:AppsFolder\\{app_id}")
    } else if crate::activities::spotify::is_spotify(app_id) {
        "spotify:".to_string()
    } else {
        return false;
    };
    let shown = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(None, windows::core::w!("open"), &HSTRING::from(target), None, None, SW_SHOWNORMAL)
    };
    // ShellExecute reports success as a value above 32.
    shown.0 as usize > 32
}

/// A window a person can switch to: (handle, exe, app id it carries, title), top of the z-order first.
struct AppWindow {
    hwnd: HWND,
    exe: String,
    aumid: Option<String>,
    title: String,
}

fn app_windows() -> Vec<AppWindow> {
    unsafe extern "system" fn each(hwnd: HWND, out: LPARAM) -> BOOL {
        let out = unsafe { &mut *(out.0 as *mut Vec<AppWindow>) };
        let tool = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32 & WS_EX_TOOLWINDOW.0 != 0;
        let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.is_invalid());
        let mut cloaked = 0u32;
        let _ = unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as _, 4) };
        if unsafe { IsWindowVisible(hwnd) }.as_bool() && !tool && !owned && cloaked == 0 {
            let mut buf = [0u16; 512];
            let len = unsafe { GetWindowTextW(hwnd, &mut buf) } as usize;
            if len > 0 {
                let exe = exe_of(hwnd).unwrap_or_default();
                out.push(AppWindow { hwnd, exe, aumid: window_aumid(hwnd), title: String::from_utf16_lossy(&buf[..len]) });
            }
        }
        true.into()
    }
    let mut out = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

/// The app id set on the window itself (Chrome, Firefox and Store app frames set one).
fn window_aumid(hwnd: HWND) -> Option<String> {
    use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd).ok()?;
        let mut value = store.GetValue(&PKEY_APP_USER_MODEL_ID).ok()?;
        let text = PropVariantToStringAlloc(&value).ok();
        let _ = PropVariantClear(&mut value);
        let text = text?;
        let s = text.to_string().ok();
        CoTaskMemFree(Some(text.0 as *const _));
        s.filter(|s| !s.is_empty())
    }
}

/// Whether a window with this exe and app id belongs to the media source `app_id`.
fn belongs(app_id: &str, exe: &str, aumid: Option<&str>) -> bool {
    let stem = |s: &str| {
        let s = s.trim();
        s.strip_suffix(".exe").or_else(|| s.strip_suffix(".EXE")).unwrap_or(s).to_ascii_lowercase()
    };
    if aumid.is_some_and(|a| a.eq_ignore_ascii_case(app_id)) {
        return true;
    }
    // "Spotify.exe" and "spotify.exe", or "Chrome"/"MSEdge" and chrome.exe/msedge.exe.
    !exe.is_empty() && !app_id.contains('!') && stem(exe) == stem(app_id)
}

fn pick(windows: &[AppWindow], app_id: &str, title: &str) -> Option<HWND> {
    let mut ours = windows.iter().filter(|w| belongs(app_id, &w.exe, w.aumid.as_deref())).peekable();
    let first = ours.peek().map(|w| w.hwnd);
    let title = title.trim();
    if title.is_empty() {
        return first;
    }
    ours.find(|w| w.title.contains(title)).map(|w| w.hwnd).or(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(n: isize, exe: &str, aumid: Option<&str>, title: &str) -> AppWindow {
        AppWindow { hwnd: HWND(n as *mut _), exe: exe.into(), aumid: aumid.map(Into::into), title: title.into() }
    }

    #[test]
    fn exe_names_match_without_case_or_extension() {
        assert!(belongs("Spotify.exe", "Spotify.exe", None));
        assert!(belongs("Chrome", "chrome.exe", None));
        assert!(belongs("MSEdge", "msedge.exe", Some("MSEdge")));
        assert!(!belongs("Spotify.exe", "chrome.exe", None));
    }

    #[test]
    fn store_ids_match_the_window_app_id_only() {
        let id = "SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify";
        assert!(belongs(id, "ApplicationFrameHost.exe", Some(id)));
        assert!(!belongs(id, "Spotify.exe", None));
        // Firefox's id is a hash, carried on its windows.
        assert!(belongs("308046B0AF4A39CB", "firefox.exe", Some("308046B0AF4A39CB")));
    }

    #[test]
    fn the_tab_playing_wins_then_the_topmost() {
        let list = [
            w(1, "Code.exe", None, "app.rs"),
            w(2, "chrome.exe", None, "Mail - Chrome"),
            w(3, "chrome.exe", None, "Lofi beats - YouTube - Chrome"),
        ];
        assert_eq!(pick(&list, "Chrome", "Lofi beats"), Some(HWND(3 as *mut _)));
        assert_eq!(pick(&list, "Chrome", "Something else"), Some(HWND(2 as *mut _)));
        assert_eq!(pick(&list, "Spotify.exe", "Lofi beats"), None);
    }
}

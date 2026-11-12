//! Port of `src/QuickByte.UI/Controls/TrayIconController.cs` and
//! `Controls/ClipboardWatcher.cs`.
//!
//! Owns the notification-area icon and its menu. QuickByte keeps running with
//! its window closed — a download manager that dies when you tidy your desktop
//! is not much of a download manager — so the tray icon is the app's only
//! remaining presence in that state, and its Exit entry is the one command that
//! really shuts the process down.
//!
//! The tray raises intent as [`TrayEvent`]s rather than acting on the manager
//! itself: every command already has exactly one implementation in the main
//! window (confirmation prompts and all), and a second copy here is how the two
//! would drift. Menu *state* (Pause All / Resume All greyed out when they could
//! do nothing) is pushed in by the UI with [`Tray::set_menu_state`].
//!
//! **The window.** One hidden window on its own thread serves the icon, its
//! balloons, its menu and the clipboard listener. It is a hidden *top-level*
//! window (a tool window that is never shown) rather than an `HWND_MESSAGE` one,
//! for two reasons that are the whole point of it: message-only windows never
//! receive broadcasts, and `TaskbarCreated` — which says Explorer restarted and
//! the icon has to be added again — is a broadcast; and `TrackPopupMenu` needs a
//! window that can take the foreground, or the menu does not close when the user
//! clicks elsewhere. It has no presence on screen or in the taskbar either way.
//!
//! **The clipboard.** There is no polling: `AddClipboardFormatListener` costs
//! nothing until something is copied, and it is registered only while the
//! feature is switched on — someone who never asked for it has nothing
//! listening to their clipboard. A copy often arrives as several updates in a
//! row, one per format, and the program that made it may still hold the
//! clipboard open for the first, so one short wait (150 ms) collapses the burst
//! and lets go of the lock before the text is read.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use quickbyte_core::events::Dispatcher;
use quickbyte_core::helpers::byte_formatter;
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RemoveClipboardFormatListener,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NIN_BALLOONHIDE, NIN_BALLOONTIMEOUT, NIN_BALLOONUSERCLICK, NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow,
    DispatchMessageW, GWLP_USERDATA, GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, HICON, HMENU,
    IDI_APPLICATION, IMAGE_ICON, KillTimer, LR_DEFAULTCOLOR, LoadIconW, LoadImageW, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG,
    PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON, SMTO_NORMAL,
    SendMessageTimeoutW, SetForegroundWindow, SetMenuDefaultItem, SetTimer, SetWindowLongPtrW, TPM_BOTTOMALIGN,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_CLIPBOARDUPDATE,
    WM_CONTEXTMENU, WM_LBUTTONDBLCLK, WM_NULL, WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::win::{copy_to_fixed, pcwstr, wide};

/// What the notification-area icon (its native context menu, its balloons) asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    /// Left click / double click / "Open QuickByte".
    Open,
    AddUrl,
    PauseAll,
    ResumeAll,
    /// The user chose Exit — the only path that closes the application for real.
    Exit,
    /// The clipboard balloon was clicked: the offered link.
    ClipboardLinkAccepted(String),
}

/// Win32 caps NOTIFYICONDATA's classic tip at 64 characters including the terminator.
const MAX_TOOLTIP_LENGTH: usize = 63;

/// The icon's callback message.
const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
/// "There are commands on the channel."
const WM_TRAY_COMMAND: u32 = WM_APP + 2;
/// Synchronous: wParam = 1 to start listening to the clipboard, 0 to stop.
const WM_TRAY_LISTEN: u32 = WM_APP + 3;
/// "Remove the icon and end the thread."
const WM_TRAY_QUIT: u32 = WM_APP + 4;

const ICON_ID: u32 = 1;
/// `NIN_SELECT | NINF_KEY`: the icon chosen with the keyboard.
const NIN_KEYSELECT: u32 = NIN_SELECT | 0x1;
const SETTLE_TIMER: usize = 1;
const SETTLE_MILLISECONDS: u32 = 150;

const ID_OPEN: u32 = 1;
const ID_ADD_URL: u32 = 2;
const ID_PAUSE_ALL: u32 = 3;
const ID_RESUME_ALL: u32 = 4;
const ID_EXIT: u32 = 5;

/// The tray icon, owned by a hidden window on its own thread. The same window
/// receives balloon clicks, `TaskbarCreated` (the icon is re-added when Explorer
/// restarts), and — while listening — clipboard updates.
pub struct Tray {
    hwnd: isize,
    commands: Sender<Command>,
    events: Receiver<TrayEvent>,
    clipboard: Receiver<String>,
    listening: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

enum Command {
    Tooltip(String),
    OfferLink { link: String, file_name: String },
    Hint { title: &'static str, body: &'static str },
    Notify { title: String, body: String },
    MenuState { can_pause: bool, can_resume: bool },
}

impl Tray {
    /// Creates the icon (QuickByte's icon from the executable's resources).
    /// `None` when the hidden window could not be created; an icon Explorer
    /// refused (it may not be running yet at sign-in) is added again when the
    /// taskbar announces itself.
    pub fn new(waker: Arc<dyn Dispatcher>) -> Option<Tray> {
        Self::create(waker, true)
    }

    /// `show_icon: false` builds everything but the notification-area icon, so
    /// the tests can exercise the clipboard listener without touching the tray.
    pub(crate) fn create(waker: Arc<dyn Dispatcher>, show_icon: bool) -> Option<Tray> {
        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (clipboard_tx, clipboard_rx) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded(1);
        let listening = Arc::new(AtomicBool::new(false));

        let state = WindowState {
            hwnd: HWND::default(),
            icon: HICON::default(),
            icon_owned: false,
            show_icon,
            taskbar_created: 0,
            tooltip: "QuickByte".into(),
            offered_link: None,
            hint_shown: false,
            can_pause: true,
            can_resume: true,
            commands: command_rx,
            events: event_tx,
            clipboard: clipboard_tx,
            listening: listening.clone(),
            waker,
        };
        let seed = Unstarted(state);
        let thread = std::thread::Builder::new()
            .name("qb-tray".into())
            .spawn(move || {
                let seed = seed;
                run_window(seed.0, ready_tx)
            })
            .ok()?;

        match ready_rx.recv() {
            Ok(Some(hwnd)) => Some(Tray {
                hwnd,
                commands: command_tx,
                events: event_rx,
                clipboard: clipboard_rx,
                listening,
                thread: Some(thread),
            }),
            _ => {
                let _ = thread.join();
                None
            }
        }
    }

    pub fn events(&self) -> crossbeam_channel::Receiver<TrayEvent> {
        self.events.clone()
    }

    /// Rewrites the hover tooltip so the tray icon reports what the app is doing
    /// while its window is closed. See [`tooltip_text`].
    pub fn update_tooltip(&self, active_count: usize, total_speed_bytes_per_second: f64) {
        self.post(Command::Tooltip(tooltip_text(active_count, total_speed_bytes_per_second)));
    }

    /// A balloon offering a copied link; clicking it sends `ClipboardLinkAccepted(link)`.
    ///
    /// A notification rather than a window: the user is in the middle of
    /// something else, copying, and a window taking focus would land their next
    /// keystrokes in QuickByte. Clicking it opens Add Download.
    pub fn offer_clipboard_link(&self, link: &str, file_name: &str) {
        self.post(Command::OfferLink { link: link.to_string(), file_name: file_name.to_string() });
    }

    /// "QuickByte is still running" when the window is closed to the tray.
    /// Explains itself once per run — the first time a close button doesn't
    /// close something is the only time it is surprising.
    pub fn notify_window_hidden(&self) {
        self.post(Command::Hint {
            title: "QuickByte is still running",
            body: "Downloads continue in the background. Right-click this icon and choose Exit to quit.",
        });
    }

    /// Called when the app was launched straight into the notification area. A
    /// launch that puts nothing on screen looks exactly like one that failed, so
    /// it says where QuickByte went — once, sharing the hint budget with
    /// [`Tray::notify_window_hidden`]: a user who has been told where the icon
    /// is does not need telling twice.
    pub fn notify_started_minimized(&self) {
        self.post(Command::Hint {
            title: "QuickByte started minimized",
            body: "It is running here in the notification area. Double-click this icon to open it.",
        });
    }

    /// A balloon about something that happened to a download (a VirusTotal
    /// warning, a download deleted for its verdict). Unlike the hints, every one
    /// is shown: each is news. *Additive to the original contract.*
    pub fn notify(&self, title: &str, body: &str) {
        self.post(Command::Notify { title: title.to_string(), body: body.to_string() });
    }

    /// Whether Pause All / Resume All can do anything right now (the C# menu
    /// computed this from `DownloadActions` as it opened). Both start enabled.
    /// *Additive to the original contract.*
    pub fn set_menu_state(&self, can_pause: bool, can_resume: bool) {
        self.post(Command::MenuState { can_pause, can_resume });
    }

    /// Starts or stops `AddClipboardFormatListener`; text arrives on [`Tray::clipboard_text`].
    /// Synchronous, so [`Tray::is_clipboard_listening`] reads the result at once.
    /// Best-effort: a refusal leaves the feature quietly off.
    pub fn set_clipboard_listening(&self, listen: bool) {
        // SAFETY: the window belongs to this Tray's thread, alive until drop.
        unsafe {
            SendMessageTimeoutW(HWND(self.hwnd as _), WM_TRAY_LISTEN, WPARAM(listen as usize), LPARAM(0), SMTO_NORMAL, 2000, None);
        }
    }

    /// Whether the clipboard listener is registered. *Additive to the original contract.*
    pub fn is_clipboard_listening(&self) -> bool {
        self.listening.load(Ordering::SeqCst)
    }

    /// The clipboard's text after each change (once a burst of changes has
    /// settled), while listening. Nothing is kept; the owner decides whether the
    /// text is a link worth offering.
    pub fn clipboard_text(&self) -> crossbeam_channel::Receiver<String> {
        self.clipboard.clone()
    }

    fn post(&self, command: Command) {
        if self.commands.send(command).is_ok() {
            // SAFETY: posting to this Tray's own window.
            unsafe {
                let _ = PostMessageW(Some(HWND(self.hwnd as _)), WM_TRAY_COMMAND, WPARAM(0), LPARAM(0));
            }
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // Hide before going: an icon whose owner process is gone lingers in the
        // notification area until something makes Windows repaint it.
        // SAFETY: posting to this Tray's own window.
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as _)), WM_TRAY_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The tip itself. Separate from the assignment because the length limit is the
/// whole point of it — a limit that is only enforced against a live tray icon is
/// one nothing can check.
pub fn tooltip_text(active_count: usize, total_speed_bytes_per_second: f64) -> String {
    let text = if active_count == 0 {
        "QuickByte — idle".to_string()
    } else {
        format!("QuickByte — {active_count} active · {}", byte_formatter::format_speed(total_speed_bytes_per_second))
    };
    if text.chars().count() > MAX_TOOLTIP_LENGTH { text.chars().take(MAX_TOOLTIP_LENGTH).collect() } else { text }
}

// ---------------------------------------------------------------------------
// The window thread
// ---------------------------------------------------------------------------

struct WindowState {
    hwnd: HWND,
    icon: HICON,
    icon_owned: bool,
    show_icon: bool,
    taskbar_created: u32,
    tooltip: String,
    /// The copied link the balloon on screen is offering, if it is offering one.
    offered_link: Option<String>,
    hint_shown: bool,
    can_pause: bool,
    can_resume: bool,
    commands: Receiver<Command>,
    events: Sender<TrayEvent>,
    clipboard: Sender<String>,
    listening: Arc<AtomicBool>,
    waker: Arc<dyn Dispatcher>,
}

/// The state on its way to the window thread, before any window or icon exists.
struct Unstarted(WindowState);

// SAFETY: at this point the handle fields are still null (HWND/HICON default);
// everything else in the state is Send. From here on the state never leaves the
// window thread.
unsafe impl Send for Unstarted {}

thread_local! {
    /// The window's state, owned by the window's thread. The window procedure
    /// borrows it per message; a message that arrives re-entrantly while it is
    /// borrowed (from inside TrackPopupMenu's modal loop, say) is left to
    /// DefWindowProc rather than risking a double borrow.
    static STATE: RefCell<Option<WindowState>> = const { RefCell::new(None) };
}

fn run_window(state: WindowState, ready: Sender<Option<isize>>) {
    let class_name = wide("QuickByte.TrayWindow");
    let title = wide("QuickByte");
    // SAFETY: standard window creation on this thread; every buffer outlives its call.
    let hwnd = unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None).map(Into::into).unwrap_or_default();
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: pcwstr(&class_name),
            ..Default::default()
        };
        // A second Tray in the same process finds the class already registered; that is fine.
        RegisterClassExW(&class);
        CreateWindowExW(
            WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0),
            pcwstr(&class_name),
            pcwstr(&title),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    };
    let Ok(hwnd) = hwnd else {
        let _ = ready.send(None);
        return;
    };

    let mut state = state;
    state.hwnd = hwnd;
    state.taskbar_created = unsafe { RegisterWindowMessageW(windows::core::w!("TaskbarCreated")) };
    let (icon, owned) = load_app_icon();
    state.icon = icon;
    state.icon_owned = owned;
    if state.show_icon {
        add_icon(&state);
    }
    STATE.with(|s| *s.borrow_mut() = Some(state));
    // SAFETY: marks the window as initialised for the window procedure.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 1);
    }
    let _ = ready.send(Some(hwnd.0 as isize));

    // SAFETY: the standard message loop for this thread's window.
    unsafe {
        let mut msg = MSG::default();
        // GetMessage returns -1 on error: stop then too, rather than spinning.
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = DestroyWindow(hwnd);
    }

    if let Some(state) = STATE.with(|s| s.borrow_mut().take())
        && state.icon_owned
    {
        // SAFETY: the icon was loaded by this thread and is no longer used.
        unsafe {
            let _ = DestroyIcon(state.icon);
        }
    }
}

/// QuickByte's icon from the executable's own resources — `build_support.rs`
/// stamps `quickbyte.ico` with winresource, whose `set_icon` uses resource id 1 —
/// at the small-icon size the notification area draws. Falls back to the stock
/// application icon for a host with no icon resource (a test binary).
fn load_app_icon() -> (HICON, bool) {
    // SAFETY: loading a resource from this module; the result is checked.
    unsafe {
        if let Ok(module) = GetModuleHandleW(None) {
            let loaded = LoadImageW(
                Some(module.into()),
                PCWSTR(1 as _),
                IMAGE_ICON,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
                LR_DEFAULTCOLOR,
            );
            if let Ok(handle) = loaded
                && !handle.is_invalid()
            {
                return (HICON(handle.0), true);
            }
        }
        (LoadIconW(None, IDI_APPLICATION).unwrap_or_default(), false)
    }
}

fn icon_data(state: &WindowState) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: state.hwnd, uID: ICON_ID, ..Default::default() }
}

fn add_icon(state: &WindowState) {
    let mut data = icon_data(state);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = WM_TRAY_CALLBACK;
    data.hIcon = state.icon;
    copy_to_fixed(&mut data.szTip, &state.tooltip);
    // SAFETY: the structure is fully initialised and outlives the calls.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &data);
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
    }
}

fn set_tooltip(state: &mut WindowState, text: String) {
    if state.tooltip == text {
        return;
    }
    state.tooltip = text;
    if !state.show_icon {
        return;
    }
    let mut data = icon_data(state);
    data.uFlags = NIF_TIP | NIF_SHOWTIP;
    copy_to_fixed(&mut data.szTip, &state.tooltip);
    // SAFETY: as in add_icon.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

fn show_balloon(state: &WindowState, timeout_ms: u32, title: &str, body: &str) {
    if !state.show_icon {
        return;
    }
    let mut data = icon_data(state);
    data.uFlags = NIF_INFO;
    data.dwInfoFlags = NIIF_INFO;
    data.Anonymous.uTimeout = timeout_ms;
    copy_to_fixed(&mut data.szInfoTitle, title);
    copy_to_fixed(&mut data.szInfo, body);
    // SAFETY: as in add_icon.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

fn emit(state: &WindowState, event: TrayEvent) {
    if state.events.send(event).is_ok() {
        state.waker.notify();
    }
}

fn handle_commands(state: &mut WindowState) {
    while let Ok(command) = state.commands.try_recv() {
        match command {
            Command::Tooltip(text) => set_tooltip(state, text),
            Command::OfferLink { link, file_name } => {
                state.offered_link = Some(link);
                show_balloon(state, 8000, "Download with QuickByte?", &format!("{file_name}\nClick to add this download."));
            }
            Command::Hint { title, body } => {
                if state.hint_shown {
                    continue;
                }
                state.hint_shown = true;
                // One balloon at a time is all Windows shows, so the click
                // belongs to whichever was raised last — a hint clears the offer
                // it replaced.
                state.offered_link = None;
                show_balloon(state, 4000, title, body);
            }
            Command::Notify { title, body } => {
                // Not budgeted like a hint: each one is news about a download.
                state.offered_link = None;
                show_balloon(state, 8000, &title, &body);
            }
            Command::MenuState { can_pause, can_resume } => {
                state.can_pause = can_pause;
                state.can_resume = can_resume;
            }
        }
    }
}

fn set_listening(state: &mut WindowState, listen: bool) {
    let listening = state.listening.load(Ordering::SeqCst);
    if listen == listening {
        return;
    }
    // SAFETY: registering this thread's own window.
    unsafe {
        if listen {
            let ok = AddClipboardFormatListener(state.hwnd).is_ok();
            state.listening.store(ok, Ordering::SeqCst);
        } else {
            let _ = RemoveClipboardFormatListener(state.hwnd);
            let _ = KillTimer(Some(state.hwnd), SETTLE_TIMER);
            state.listening.store(false, Ordering::SeqCst);
        }
    }
}

/// The native popup menu at `anchor` (or the cursor); what was chosen, if anything.
fn show_menu(hwnd: HWND, can_pause: bool, can_resume: bool, anchor: POINT) -> Option<TrayEvent> {
    // SAFETY: a popup menu owned and destroyed by this function, tracked on this
    // thread's own window.
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let item = |menu: HMENU, id: u32, text: &str, enabled: bool| {
            let text = wide(text);
            let flags = if enabled { MF_STRING } else { MF_STRING | MF_GRAYED };
            let _ = AppendMenuW(menu, flags, id as usize, pcwstr(&text));
        };
        item(menu, ID_OPEN, "&Open QuickByte", true);
        item(menu, ID_ADD_URL, "Add &URL...", true);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        item(menu, ID_PAUSE_ALL, "&Pause All", can_pause);
        item(menu, ID_RESUME_ALL, "&Resume All", can_resume);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        item(menu, ID_EXIT, "E&xit", true);
        // Bold: the default action, and the one a double-click performs.
        let _ = SetMenuDefaultItem(menu, ID_OPEN, 0);

        let mut point = anchor;
        if point.x == 0 && point.y == 0 {
            let _ = GetCursorPos(&mut point);
        }
        // Without taking the foreground first the menu would not close when the
        // user clicks elsewhere; the WM_NULL afterwards is the documented other half.
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RETURNCMD, point.x, point.y, None, hwnd, None).0 as u32;
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);

        match chosen {
            ID_OPEN => Some(TrayEvent::Open),
            ID_ADD_URL => Some(TrayEvent::AddUrl),
            ID_PAUSE_ALL => Some(TrayEvent::PauseAll),
            ID_RESUME_ALL => Some(TrayEvent::ResumeAll),
            ID_EXIT => Some(TrayEvent::Exit),
            _ => None,
        }
    }
}

/// The clipboard's Unicode text, or `None` when it holds none — or when another
/// program is holding the clipboard open (tried a few times, then given up).
fn read_clipboard_text(hwnd: HWND) -> Option<String> {
    // SAFETY: the clipboard is opened, read under GlobalLock and closed again on
    // every path; the text is copied out before the lock is released.
    unsafe {
        IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).ok()?;
        let mut opened = false;
        for _ in 0..5 {
            if OpenClipboard(Some(hwnd)).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        if !opened {
            return None;
        }
        let text = (|| {
            let handle: HANDLE = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            let global = HGLOBAL(handle.0);
            let pointer = GlobalLock(global) as *const u16;
            if pointer.is_null() {
                return None;
            }
            let capacity = GlobalSize(global) / 2;
            let units = std::slice::from_raw_parts(pointer, capacity);
            let length = units.iter().position(|&u| u == 0).unwrap_or(capacity);
            let text = String::from_utf16_lossy(&units[..length]);
            let _ = GlobalUnlock(global);
            Some(text)
        })();
        let _ = CloseClipboard();
        text
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: reading this window's own user data.
    let initialised = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } != 0;

    // The menu runs a modal message loop, so it is shown *without* the state
    // borrowed: messages that arrive meanwhile (a command, the clipboard switch)
    // are then handled normally instead of being dropped.
    if initialised && message == WM_TRAY_CALLBACK && (lparam.0 as u32) & 0xFFFF == WM_CONTEXTMENU {
        let x = (wparam.0 & 0xFFFF) as u16 as i16 as i32;
        let y = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
        let menu_state = STATE.with(|cell| cell.try_borrow().ok()?.as_ref().map(|s| (s.can_pause, s.can_resume)));
        if let Some((can_pause, can_resume)) = menu_state
            && let Some(event) = show_menu(hwnd, can_pause, can_resume, POINT { x, y })
        {
            STATE.with(|cell| {
                if let Ok(borrowed) = cell.try_borrow()
                    && let Some(state) = borrowed.as_ref()
                {
                    emit(state, event);
                }
            });
        }
        return LRESULT(0);
    }

    if initialised
        && let Some(result) = STATE.with(|cell| {
            let mut borrowed = cell.try_borrow_mut().ok()?;
            let state = borrowed.as_mut()?;
            handle_message(state, message, wparam, lparam)
        })
    {
        return result;
    }
    // SAFETY: default processing for everything this window does not handle.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn handle_message(state: &mut WindowState, message: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    match message {
        WM_TRAY_COMMAND => handle_commands(state),
        WM_TRAY_LISTEN => set_listening(state, wparam.0 != 0),
        WM_TRAY_CALLBACK => {
            // NOTIFYICON_VERSION_4: the event is LOWORD(lParam), the anchor point is in wParam.
            let event = (lparam.0 as u32) & 0xFFFF;
            match event {
                NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONDBLCLK => emit(state, TrayEvent::Open),
                NIN_BALLOONUSERCLICK => {
                    if let Some(link) = state.offered_link.take() {
                        emit(state, TrayEvent::ClipboardLinkAccepted(link));
                    }
                }
                NIN_BALLOONTIMEOUT | NIN_BALLOONHIDE => state.offered_link = None,
                _ => {}
            }
        }
        WM_CLIPBOARDUPDATE => {
            if state.listening.load(Ordering::SeqCst) {
                // Restarts the settle timer: a burst of updates reads once.
                // SAFETY: a timer on this thread's own window.
                unsafe {
                    SetTimer(Some(state.hwnd), SETTLE_TIMER, SETTLE_MILLISECONDS, None);
                }
            }
        }
        WM_TIMER if wparam.0 == SETTLE_TIMER => {
            // SAFETY: as above.
            unsafe {
                let _ = KillTimer(Some(state.hwnd), SETTLE_TIMER);
            }
            if state.listening.load(Ordering::SeqCst)
                && let Some(text) = read_clipboard_text(state.hwnd)
                && state.clipboard.send(text).is_ok()
            {
                state.waker.notify();
            }
        }
        WM_TRAY_QUIT => {
            set_listening(state, false);
            if state.show_icon {
                let data = icon_data(state);
                // SAFETY: as in add_icon.
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &data);
                }
                state.show_icon = false;
            }
            // SAFETY: ends this thread's message loop; the window is destroyed
            // after the loop, outside any borrow of the state.
            unsafe { PostQuitMessage(0) };
        }
        m if m != 0 && m == state.taskbar_created => {
            // Explorer restarted: every notification-area icon is gone and has
            // to be added again.
            if state.show_icon {
                add_icon(state);
            }
        }
        _ => return None,
    }
    Some(LRESULT(0))
}

#[cfg(test)]
mod tests {
    //! Port of `TrayIconControllerTests.cs` (the tooltip) and
    //! `ClipboardWatcherTests.cs` (the listener's switch). Nothing here copies
    //! anything — a test run has no business replacing whatever the user last
    //! copied — and no notification-area icon is created.

    use quickbyte_core::events::NoopDispatcher;

    use super::*;

    #[test]
    fn an_idle_app_says_so() {
        assert_eq!(tooltip_text(0, 0.0), "QuickByte — idle");
    }

    #[test]
    fn an_active_app_reports_what_it_is_doing() {
        let text = tooltip_text(3, 1024.0 * 1024.0);
        assert!(text.starts_with("QuickByte — 3 active · "), "{text}");
        assert!(text.contains("/s"));
    }

    #[test]
    fn the_speed_is_dropped_when_nothing_is_running() {
        assert!(!tooltip_text(0, 12345.0).contains("active"));
    }

    #[test]
    fn the_tip_never_exceeds_what_windows_accepts() {
        for (active, speed) in [(0, 0.0), (1, 1024.0), (32, 12_345_678.0), (9999, 9.9e12), (usize::MAX, f64::MAX)] {
            assert!(tooltip_text(active, speed).chars().count() <= MAX_TOOLTIP_LENGTH);
        }
    }

    #[test]
    fn the_widest_tip_the_app_can_build_is_inside_the_limit() {
        let widest = tooltip_text(i32::MAX as usize, i64::MAX as f64);
        assert!((1..=MAX_TOOLTIP_LENGTH - 10).contains(&widest.chars().count()), "{widest}");
    }

    #[test]
    fn the_count_is_the_number_it_was_given() {
        assert!(tooltip_text(7, 2048.0).contains('7'));
    }

    fn watcher() -> Tray {
        Tray::create(Arc::new(NoopDispatcher), false).expect("the hidden window")
    }

    #[test]
    fn a_new_watcher_is_not_listening() {
        assert!(!watcher().is_clipboard_listening());
    }

    #[test]
    fn listening_can_be_switched_on_and_off_again() {
        let tray = watcher();

        tray.set_clipboard_listening(true);
        assert!(tray.is_clipboard_listening());

        tray.set_clipboard_listening(false);
        assert!(!tray.is_clipboard_listening());
    }

    #[test]
    fn switching_it_on_twice_registers_once_and_off_once_is_enough() {
        let tray = watcher();

        tray.set_clipboard_listening(true);
        tray.set_clipboard_listening(true);
        tray.set_clipboard_listening(false);

        assert!(!tray.is_clipboard_listening());
    }

    #[test]
    fn dropping_a_listening_watcher_stops_it() {
        let tray = watcher();
        tray.set_clipboard_listening(true);
        let listening = tray.listening.clone();

        drop(tray);

        assert!(!listening.load(Ordering::SeqCst));
    }

    #[test]
    fn commands_reach_the_window_thread_without_an_icon() {
        let tray = watcher();
        tray.update_tooltip(2, 2048.0);
        tray.set_menu_state(false, true);
        tray.notify_window_hidden();
        tray.notify_started_minimized();
        tray.offer_clipboard_link("https://example.com/a.zip", "a.zip");
        // A round trip through the window proves the posted commands were handled.
        tray.set_clipboard_listening(false);
        assert!(tray.events().try_recv().is_err());
    }
}

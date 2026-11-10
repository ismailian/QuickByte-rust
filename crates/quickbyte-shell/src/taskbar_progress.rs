//! Port of `src/QuickByte.UI/Controls/TaskbarProgress.cs`.
//!
//! Paints a window's taskbar button with the download's progress — the bar
//! behind the icon that a browser, a copy dialog and every other download
//! manager show, so a download that is minimised or sitting behind the browser
//! can still be read at a glance.
//!
//! Three things here are not obvious and all three are load-bearing:
//!
//! - **The button does not exist when the window is created.** The shell builds
//!   it after the window is first shown; every call made before that is silently
//!   dropped. A download that is moving papers over this by reporting again
//!   100 ms later, but a paused, failed or queued one never sends a second
//!   report — its indicator would simply never appear. The C# class re-applied
//!   the cached report on `TaskbarButtonCreated`, which it saw by subclassing
//!   the form. An egui window belongs to winit, so instead the last report is
//!   cached and re-asserted for a few seconds after attaching and after each
//!   time the window becomes visible again (hiding a window takes its taskbar
//!   button with it — "close to tray" does exactly that).
//! - **State before value.** `TBPF_NOPROGRESS` and `TBPF_INDETERMINATE` discard
//!   the value, and writing a value while the button is indeterminate turns it
//!   back into a determinate bar — so the value is written only for the three
//!   states that have one.
//! - **It is a decoration.** A shell that will not answer must not take the
//!   window down with it, so every failure is swallowed and the object goes inert.
//!
//! All COM lives on one small background thread (an STA that owns the shell's
//! `ITaskbarList3`, created once for the process — every method on it takes an
//! HWND, so there is nothing per-window about it). That keeps [`TaskbarProgress`]
//! `Send + Sync`, so it can sit in an egui deferred viewport's state.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use quickbyte_core::enums::DownloadStatus;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::UI::Shell::{ITaskbarList3, TBPFLAG, TaskbarList};
use windows::Win32::UI::WindowsAndMessaging::{IsWindow, IsWindowVisible};

/// The looks a taskbar button's progress indicator can wear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskbarProgressState {
    /// No indicator at all — the button paints as an ordinary one.
    None,
    /// A marquee. Carries no value; use it when there is nothing to measure.
    Indeterminate,
    /// The green bar.
    Normal,
    /// The yellow bar.
    Paused,
    /// The red bar.
    Error,
}

impl TaskbarProgressState {
    /// The shell's own `TBPFLAG` bits.
    fn flag(self) -> TBPFLAG {
        TBPFLAG(match self {
            Self::None => 0x0,
            Self::Indeterminate => 0x1,
            Self::Normal => 0x2,
            Self::Error => 0x4,
            Self::Paused => 0x8,
        })
    }

    fn to_u8(self) -> u8 {
        self.flag().0 as u8
    }

    fn from_u8(v: u8) -> Self {
        match v {
            0x1 => Self::Indeterminate,
            0x2 => Self::Normal,
            0x4 => Self::Error,
            0x8 => Self::Paused,
            _ => Self::None,
        }
    }

    fn has_value(self) -> bool {
        matches!(self, Self::Normal | Self::Paused | Self::Error)
    }
}

/// Progress is reported out of 1000 rather than 100: the button is wide enough
/// that whole-percent steps read as jumps on a large file.
const PROGRESS_RESOLUTION: u64 = 1000;

/// How many one-second ticks a report is re-asserted for after attaching or
/// after the window reappears — long enough for the shell to build the button.
const REASSERT_TICKS: u8 = 5;

/// `ITaskbarList3` progress on one window's taskbar button. `hwnd` is the raw window handle.
pub struct TaskbarProgress {
    shared: Arc<Shared>,
}

struct Shared {
    hwnd: isize,
    state: AtomicU8,
    value: AtomicU64,
}

impl TaskbarProgress {
    /// Starts driving `hwnd`'s taskbar button. Safe to call before the window is
    /// shown — the indicator is applied once the window, and then its button, exist.
    /// `None` for a handle that is not a window.
    pub fn attach_to(hwnd: isize) -> Option<TaskbarProgress> {
        // SAFETY: IsWindow accepts any value.
        if hwnd == 0 || !unsafe { IsWindow(Some(HWND(hwnd as _))) }.as_bool() {
            return None;
        }
        let shared = Arc::new(Shared { hwnd, state: AtomicU8::new(TaskbarProgressState::None.to_u8()), value: AtomicU64::new(0) });
        send(Message::Attach(shared.clone()));
        Some(TaskbarProgress { shared })
    }

    /// `percentage` 0–100; ignored for None / Indeterminate. A repeated identical
    /// report costs nothing, which matters because progress arrives ten times a second.
    pub fn report(&mut self, state: TaskbarProgressState, percentage: f64) {
        let percentage = if percentage.is_nan() { 0.0 } else { percentage.clamp(0.0, 100.0) };
        let value = (percentage * (PROGRESS_RESOLUTION as f64 / 100.0)).round() as u64;
        let old_state = self.shared.state.swap(state.to_u8(), Ordering::SeqCst);
        let old_value = self.shared.value.swap(value, Ordering::SeqCst);
        if old_state == state.to_u8() && old_value == value {
            return;
        }
        send(Message::Apply(self.shared.clone()));
    }

    /// Removes the indicator, leaving an ordinary taskbar button.
    pub fn clear(&mut self) {
        self.report(TaskbarProgressState::None, 0.0);
    }
}

impl Drop for TaskbarProgress {
    fn drop(&mut self) {
        send(Message::Detach(self.shared.clone()));
    }
}

/// Green while transferring, yellow when paused, red on failure, a marquee while
/// connecting or when the size is unknown.
///
/// Split out from [`TaskbarProgress::report`] so it can be tested: the mapping is
/// the half of this that can be wrong without anything failing — an indicator
/// that never clears, or a finished download still tinting its button red, is
/// not an error anywhere.
///
/// A size the server never gave leaves `percentage` permanently zero, so those
/// downloads get the marquee rather than a bar stuck at the left edge. Merging
/// keeps the full bar for the reason the window's own bar does — every byte is
/// already on disk, and rewinding to report the merge would be a lie.
pub fn state_for(status: DownloadStatus, size_known: bool, percentage: f64) -> TaskbarProgressState {
    match status {
        DownloadStatus::Failed => TaskbarProgressState::Error,
        DownloadStatus::Paused => TaskbarProgressState::Paused,
        DownloadStatus::Connecting if percentage <= 0.0 => TaskbarProgressState::Indeterminate,
        DownloadStatus::Connecting | DownloadStatus::Downloading | DownloadStatus::Merging | DownloadStatus::Verifying => {
            if size_known {
                TaskbarProgressState::Normal
            } else {
                TaskbarProgressState::Indeterminate
            }
        }
        _ => TaskbarProgressState::None,
    }
}

// -------------------------------------------------------------- Plumbing --

enum Message {
    Attach(Arc<Shared>),
    Apply(Arc<Shared>),
    Detach(Arc<Shared>),
}

fn send(message: Message) {
    static WORKER: OnceLock<Option<Sender<Message>>> = OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::Builder::new().name("qb-taskbar-progress".into()).spawn(move || run(rx)).ok().map(|_| tx)
    });
    if let Some(tx) = worker {
        let _ = tx.send(message);
    }
}

struct Entry {
    shared: Arc<Shared>,
    visible: bool,
    reasserts_left: u8,
}

fn run(rx: Receiver<Message>) {
    // SAFETY: this thread's own apartment, initialised once and kept for its life.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let mut shell = Shell::default();
    let mut entries: Vec<Entry> = Vec::new();

    loop {
        let message = if entries.is_empty() {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            rx.recv_timeout(Duration::from_secs(1))
        };
        match message {
            Ok(Message::Attach(shared)) => {
                shell.apply(&shared);
                entries.push(Entry { visible: is_visible(shared.hwnd), shared, reasserts_left: REASSERT_TICKS });
            }
            Ok(Message::Apply(shared)) => shell.apply(&shared),
            Ok(Message::Detach(shared)) => {
                entries.retain(|e| !Arc::ptr_eq(&e.shared, &shared));
                if is_window(shared.hwnd) {
                    shared.state.store(TaskbarProgressState::None.to_u8(), Ordering::SeqCst);
                    shared.value.store(0, Ordering::SeqCst);
                    shell.apply(&shared);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                entries.retain(|e| is_window(e.shared.hwnd));
                for entry in &mut entries {
                    let visible = is_visible(entry.shared.hwnd);
                    // Hiding a window takes its taskbar button away with it.
                    // Re-assert on the way back rather than trusting the shell
                    // to have remembered.
                    if visible && !entry.visible {
                        entry.reasserts_left = REASSERT_TICKS;
                    }
                    entry.visible = visible;
                    if visible && entry.reasserts_left > 0 {
                        entry.reasserts_left -= 1;
                        shell.apply(&entry.shared);
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn is_window(hwnd: isize) -> bool {
    // SAFETY: IsWindow accepts any value.
    unsafe { IsWindow(Some(HWND(hwnd as _))) }.as_bool()
}

fn is_visible(hwnd: isize) -> bool {
    // SAFETY: IsWindowVisible accepts any value.
    unsafe { IsWindowVisible(HWND(hwnd as _)) }.as_bool()
}

/// The shell's TaskbarList object, created on first use; inert once it fails.
#[derive(Default)]
struct Shell {
    list: Option<ITaskbarList3>,
    unavailable: bool,
}

impl Shell {
    fn get(&mut self) -> Option<&ITaskbarList3> {
        if self.list.is_none() && !self.unavailable {
            // SAFETY: COM is initialised on this thread (see `run`).
            let created: windows::core::Result<ITaskbarList3> = unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER) };
            match created.and_then(|list| unsafe { list.HrInit() }.map(|_| list)) {
                Ok(list) => self.list = Some(list),
                Err(_) => self.unavailable = true,
            }
        }
        self.list.as_ref()
    }

    fn apply(&mut self, shared: &Shared) {
        let state = TaskbarProgressState::from_u8(shared.state.load(Ordering::SeqCst));
        let value = shared.value.load(Ordering::SeqCst);
        let hwnd = HWND(shared.hwnd as _);
        let Some(list) = self.get() else { return };
        // SAFETY: COM calls on this thread's own object with a window handle.
        let result = unsafe {
            list.SetProgressState(hwnd, state.flag()).and_then(|_| {
                if state.has_value() { list.SetProgressValue(hwnd, value, PROGRESS_RESOLUTION) } else { Ok(()) }
            })
        };
        if result.is_err() && !is_window(shared.hwnd) {
            return; // The window went away mid-call; nothing wrong with the shell.
        }
        if result.is_err() {
            self.list = None;
            self.unavailable = true;
        }
    }
}

#[cfg(test)]
mod tests {
    //! Port of `tests/QuickByte.UI.Tests/TaskbarProgressTests.cs`: the
    //! download-status-to-indicator mapping, where the bugs with no symptom live.

    use super::*;

    #[test]
    fn a_download_in_flight_shows_a_bar() {
        for status in [DownloadStatus::Downloading, DownloadStatus::Connecting, DownloadStatus::Merging, DownloadStatus::Verifying] {
            assert_eq!(state_for(status, true, 42.0), TaskbarProgressState::Normal, "{status:?}");
        }
    }

    #[test]
    fn a_merge_keeps_the_bar_full() {
        assert_eq!(state_for(DownloadStatus::Merging, true, 100.0), TaskbarProgressState::Normal);
    }

    #[test]
    fn a_download_of_unknown_size_gets_the_marquee() {
        assert_eq!(state_for(DownloadStatus::Downloading, false, 0.0), TaskbarProgressState::Indeterminate);
    }

    #[test]
    fn connecting_with_nothing_downloaded_yet_gets_the_marquee() {
        assert_eq!(state_for(DownloadStatus::Connecting, true, 0.0), TaskbarProgressState::Indeterminate);
    }

    #[test]
    fn connecting_to_resume_keeps_the_progress_already_made() {
        assert_eq!(state_for(DownloadStatus::Connecting, true, 80.0), TaskbarProgressState::Normal);
    }

    #[test]
    fn a_paused_download_turns_the_bar_yellow() {
        assert_eq!(state_for(DownloadStatus::Paused, true, 30.0), TaskbarProgressState::Paused);
    }

    #[test]
    fn a_paused_download_of_unknown_size_still_reads_as_paused() {
        assert_eq!(state_for(DownloadStatus::Paused, false, 0.0), TaskbarProgressState::Paused);
    }

    #[test]
    fn a_failed_download_turns_the_bar_red() {
        assert_eq!(state_for(DownloadStatus::Failed, true, 30.0), TaskbarProgressState::Error);
    }

    #[test]
    fn a_download_that_is_not_running_leaves_the_button_alone() {
        for status in [DownloadStatus::Completed, DownloadStatus::Cancelled, DownloadStatus::Queued] {
            assert_eq!(state_for(status, true, 100.0), TaskbarProgressState::None, "{status:?}");
        }
    }

    #[test]
    fn the_states_are_the_shells_own_flags() {
        for state in [
            TaskbarProgressState::None,
            TaskbarProgressState::Indeterminate,
            TaskbarProgressState::Normal,
            TaskbarProgressState::Paused,
            TaskbarProgressState::Error,
        ] {
            assert_eq!(TaskbarProgressState::from_u8(state.to_u8()), state);
        }
        assert_eq!(TaskbarProgressState::Paused.flag().0, 0x8);
        assert_eq!(TaskbarProgressState::Error.flag().0, 0x4);
    }

    #[test]
    fn something_that_is_not_a_window_gets_no_indicator() {
        assert!(TaskbarProgress::attach_to(0).is_none());
    }

    #[test]
    fn the_indicator_is_send_and_sync_for_a_deferred_viewport() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TaskbarProgress>();
    }
}

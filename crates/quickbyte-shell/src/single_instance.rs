//! Port of `src/QuickByte.UI/SingleInstance.cs` (and `Program.HasMinimizedSwitch`).
//!
//! Keeps QuickByte to one running copy per user, the way a download manager is
//! expected to behave: the app owns a persistent download list and a temp-folder
//! tree, and two processes writing `downloads.json` would quietly clobber each
//! other's state.
//!
//! A named mutex decides who wins; a named pipe gives the loser somewhere to
//! hand its command line before it exits, so launching QuickByte with a URL
//! while it is already running adds the download to the window you already have
//! instead of starting a rival process.
//!
//! Hand-offs arrive on a background thread and are queued on a channel; the
//! `waker` is told after each one, so the UI drains [`SingleInstance::payloads`]
//! on its own thread — the Rust shape of marshalling `SecondInstanceStarted`
//! through `AppDispatcher`.
//!
//! The names and the wire protocol are the C# build's: write the payload (UTF-8)
//! and close, no reply. An old C# second launch can hand off to a Rust instance,
//! and a Rust second launch to an old C# instance.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use quickbyte_core::events::Dispatcher;
use uuid::Uuid;
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, GENERIC_WRITE, GetLastError, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
    ReadFile, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects};

use crate::win::{OwnedHandle, pcwstr, user_name, wide};

// "Local\" scopes the mutex to the logon session, which is what we want: two
// users on the same machine each get their own QuickByte.
pub const MUTEX_NAME: &str = r"Local\QuickByte.SingleInstance";
/// The command-line switch the scheduler agent launches QuickByte with when a
/// queue comes due, and — as `run-queue:{guid}` — the shape that request takes
/// when it is handed to an instance that is already running.
pub const RUN_QUEUE_SWITCH: &str = "--run-queue";
pub const QUEUE_PAYLOAD_PREFIX: &str = "run-queue:";

const HANDOFF_TIMEOUT: Duration = Duration::from_millis(3000);

/// A hand-off is a URL or a queue id; anything past this is not one, and is not
/// worth holding the listener for.
const MAX_PAYLOAD_BYTES: usize = 1 << 20;

/// Named pipes are machine-wide even when the mutex is not, so the user name
/// has to be part of the pipe name for the two to agree on scope.
fn pipe_name() -> String {
    format!(r"\\.\pipe\QuickByte.SingleInstance.{}{}", user_name(), quickbyte_core::helpers::app_paths::instance_suffix())
}

/// One QuickByte per user: a named mutex (`Local\QuickByte.SingleInstance`) and a
/// hand-off pipe (`\\.\pipe\QuickByte.SingleInstance.<UserName>`). Same names as the
/// C# build, so an old and a new copy can never run side by side.
pub struct SingleInstance {
    mutex: isize,
    stop_event: isize,
    payloads: Receiver<String>,
    listener: Option<JoinHandle<()>>,
}

impl SingleInstance {
    /// Takes the mutex and starts listening for hand-offs; `None` when another copy holds it.
    /// `waker` is notified after each payload is queued.
    ///
    /// The mutex is deliberately created unowned: existence is the signal, so
    /// there is no ownership to release on the wrong thread and nothing to
    /// abandon if the process is killed.
    pub fn acquire(waker: Arc<dyn Dispatcher>) -> Option<SingleInstance> {
        // A developer sandbox (QUICKBYTE_SANDBOX) gets names of its own, so it
        // never hands its launch to the installed copy.
        let mutex = format!("{MUTEX_NAME}{}", quickbyte_core::helpers::app_paths::instance_suffix());
        Self::acquire_named(&mutex, &pipe_name(), waker)
    }

    pub(crate) fn acquire_named(mutex_name: &str, pipe_name: &str, waker: Arc<dyn Dispatcher>) -> Option<SingleInstance> {
        let name = wide(mutex_name);
        // SAFETY: the name buffer outlives the call.
        let mutex = unsafe { CreateMutexW(None, false, pcwstr(&name)) }.ok()?;
        // SAFETY: read straight after the call that set it.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            drop(OwnedHandle(mutex));
            return None;
        }

        // SAFETY: a manual-reset, unnamed event.
        let stop_event = match unsafe { CreateEventW(None, true, false, None) } {
            Ok(event) => event,
            Err(_) => {
                drop(OwnedHandle(mutex));
                return None;
            }
        };

        let (tx, rx) = crossbeam_channel::unbounded();
        let pipe = wide(pipe_name);
        let stop = stop_event.0 as isize;
        let listener = std::thread::Builder::new()
            .name("qb-single-instance".into())
            .spawn(move || listen(&pipe, HANDLE(stop as _), &tx, waker.as_ref()))
            .ok();

        Some(SingleInstance { mutex: mutex.0 as isize, stop_event: stop, payloads: rx, listener })
    }

    /// Payloads handed over by later launches: a URL, `run-queue:<guid>`, or ""
    /// (a launch with nothing to say — bring the window up).
    pub fn payloads(&self) -> crossbeam_channel::Receiver<String> {
        self.payloads.clone()
    }

    /// Hands `payload` to the running copy (3 s connect timeout). Write-then-close; no reply.
    ///
    /// Returns false if nobody answered — the owner may be shutting down, in which
    /// case the caller has nothing useful left to do but exit anyway.
    pub fn send_to_running_instance(payload: &str) -> bool {
        send_to(&pipe_name(), payload, HANDOFF_TIMEOUT)
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: the event is owned by this value until it is closed below.
        unsafe {
            let _ = SetEvent(HANDLE(self.stop_event as _));
        }
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
        drop(OwnedHandle(HANDLE(self.stop_event as _)));
        drop(OwnedHandle(HANDLE(self.mutex as _)));
    }
}

/// The listener: one pipe instance at a time (`maxNumberOfServerInstances: 1`),
/// overlapped so that dropping [`SingleInstance`] can end it mid-wait.
fn listen(pipe_name: &[u16], stop: HANDLE, tx: &Sender<String>, waker: &dyn Dispatcher) {
    loop {
        match serve_one(pipe_name, stop) {
            Served::Payload(payload) => {
                if tx.send(payload).is_ok() {
                    waker.notify();
                }
            }
            // A half-open connection or a malformed payload must not take the
            // listener down — the next launch still deserves to be answered.
            Served::Nothing => {}
            Served::CouldNotListen => {
                // The name is taken (a rival listener) or the system refused: try
                // again shortly rather than spinning.
                // SAFETY: waiting on an event this thread does not own but which outlives it.
                if unsafe { WaitForMultipleObjects(&[stop], false, 1000) } == WAIT_OBJECT_0 {
                    return;
                }
            }
            Served::Stopped => return,
        }
    }
}

enum Served {
    Payload(String),
    Nothing,
    CouldNotListen,
    Stopped,
}

fn serve_one(pipe_name: &[u16], stop: HANDLE) -> Served {
    // SAFETY: plain Win32 pipe calls on handles owned by this function; the
    // OVERLAPPED and its event outlive every operation that uses them (each one
    // is either completed or cancelled and waited for before they are dropped).
    unsafe {
        let pipe = CreateNamedPipeW(
            pcwstr(pipe_name),
            FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_INBOUND.0 | FILE_FLAG_OVERLAPPED.0),
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            0,
            4096,
            0,
            None,
        );
        if pipe.is_invalid() {
            return Served::CouldNotListen;
        }
        let pipe = OwnedHandle(pipe);
        let Ok(event) = CreateEventW(None, true, false, None) else { return Served::CouldNotListen };
        let event = OwnedHandle(event);
        let mut overlapped = OVERLAPPED { hEvent: event.0, ..Default::default() };

        // Wait for a client.
        match ConnectNamedPipe(pipe.0, Some(&mut overlapped)) {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                if !wait_for(pipe.0, &overlapped, stop) {
                    return Served::Stopped;
                }
                let mut ignored = 0;
                if GetOverlappedResult(pipe.0, &overlapped, &mut ignored, false).is_err() {
                    return Served::Nothing;
                }
            }
            Err(_) => return Served::Nothing,
        }

        // Read to the end: the client writes its payload and closes.
        let mut payload = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let _ = ResetEvent(event.0);
            let mut read = 0u32;
            let result = ReadFile(pipe.0, Some(&mut buffer), None, Some(&mut overlapped));
            let finished = match result {
                Ok(()) => GetOverlappedResult(pipe.0, &overlapped, &mut read, false),
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                    if !wait_for(pipe.0, &overlapped, stop) {
                        return Served::Stopped;
                    }
                    GetOverlappedResult(pipe.0, &overlapped, &mut read, false)
                }
                Err(e) => Err(e),
            };
            payload.extend_from_slice(&buffer[..(read as usize).min(buffer.len())]);
            match finished {
                // A zero-byte read on a byte pipe is a zero-length write, not a close.
                Ok(()) => {}
                Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => {}
                // ERROR_BROKEN_PIPE: the client wrote and closed — the whole protocol.
                Err(e) if e.code() == ERROR_BROKEN_PIPE.to_hresult() => break,
                Err(_) => break,
            }
            if payload.len() > MAX_PAYLOAD_BYTES {
                break;
            }
        }
        let _ = DisconnectNamedPipe(pipe.0);

        // StreamReader would skip a BOM; .NET's StreamWriter writes none.
        let text = String::from_utf8_lossy(quickbyte_core::serde_compat::strip_bom(&payload));
        Served::Payload(text.trim().to_string())
    }
}

/// Waits for an overlapped operation or the stop event. On stop, cancels the
/// operation and waits for the cancellation to land before returning false.
///
/// SAFETY (callers): `overlapped` must be the in-flight operation on `handle`.
unsafe fn wait_for(handle: HANDLE, overlapped: &OVERLAPPED, stop: HANDLE) -> bool {
    // SAFETY: both handles are live for the duration of the wait.
    let woke = unsafe { WaitForMultipleObjects(&[overlapped.hEvent, stop], false, INFINITE) };
    if woke == WAIT_OBJECT_0 {
        return true;
    }
    // SAFETY: cancelling and draining the caller's own operation.
    unsafe {
        let _ = CancelIoEx(handle, Some(overlapped));
        let mut ignored = 0;
        let _ = GetOverlappedResult(handle, overlapped, &mut ignored, true);
    }
    false
}

/// The client half: `NamedPipeClientStream.Connect(timeout)` (which keeps trying
/// while the pipe does not exist or is busy, until the timeout), then
/// write-and-close.
pub(crate) fn send_to(pipe_name: &str, payload: &str, timeout: Duration) -> bool {
    let name = wide(pipe_name);
    let deadline = Instant::now() + timeout;
    let handle = loop {
        // SAFETY: the name buffer outlives the call.
        let opened = unsafe {
            CreateFileW(pcwstr(&name), GENERIC_WRITE.0, FILE_SHARE_NONE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None)
        };
        match opened {
            Ok(handle) => break OwnedHandle(handle),
            Err(e) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                if e.code() == ERROR_PIPE_BUSY.to_hresult() {
                    // SAFETY: the name buffer outlives the call.
                    let _ = unsafe { WaitNamedPipeW(pcwstr(&name), remaining.as_millis().max(1) as u32) };
                } else if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() {
                    // The owner is between two pipe instances; try again shortly.
                    std::thread::sleep(remaining.min(Duration::from_millis(20)));
                } else {
                    return false;
                }
            }
        }
    };

    let mut bytes = payload.as_bytes();
    while !bytes.is_empty() {
        let mut written = 0u32;
        // SAFETY: the buffer and counter outlive this synchronous write.
        if unsafe { WriteFile(handle.0, Some(bytes), Some(&mut written), None) }.is_err() || written == 0 {
            return false;
        }
        bytes = &bytes[written as usize..];
    }
    true
}

/// What a losing launch sends to the winner: the queue it was asked to start,
/// or the link it was given, or nothing at all (`""`, not absent: the payload is
/// written to a pipe, and the receiving side reads "no argument" as empty).
///
/// One payload rather than two channels because the pipe carries one string and
/// the receiver has to be able to tell the two apart — a URL is recognized by
/// its scheme, a queue by this prefix.
pub fn build_handoff_payload(args: &[String]) -> String {
    if let Some(queue_id) = find_queue_id(args) {
        return format!("{QUEUE_PAYLOAD_PREFIX}{}", queue_id.hyphenated());
    }
    find_url(args).unwrap_or_default()
}

/// The queue id in a command line or in a handed-over payload: `--run-queue
/// {guid}` as the agent passes it, or `run-queue:{guid}` as it crosses the pipe.
pub fn find_queue_id(args: &[String]) -> Option<Uuid> {
    let tokens: Vec<&str> = args.iter().map(|a| clean(a)).collect();
    for (i, token) in tokens.iter().enumerate() {
        if let Some(rest) = strip_prefix_ignore_case(token, QUEUE_PAYLOAD_PREFIX)
            && let Some(id) = parse_guid(rest)
        {
            return Some(id);
        }
        if !token.eq_ignore_ascii_case(RUN_QUEUE_SWITCH) {
            continue;
        }
        if let Some(id) = tokens.get(i + 1).and_then(|next| parse_guid(next)) {
            return Some(id);
        }
    }
    None
}

/// The first http/https/ftp/ftps absolute URL, ignoring switches and anything
/// that is not a link. Used both to decide what a second launch should hand
/// over and to validate what arrives.
///
/// The scheme list is the same one the Add dialog accepts, FTP included — an
/// ftp:// link handed over by the shell has to reach the dialog rather than
/// being silently dropped as "not a URL".
pub fn find_url(args: &[String]) -> Option<String> {
    args.iter().map(|a| clean(a)).find(|a| is_download_url(a)).map(str::to_string)
}

/// `--minimized`, `-minimized`, `/minimized` or `-m` (Program.HasMinimizedSwitch).
/// Matched case-sensitively, unlike `--run-queue`: everything that writes this
/// switch writes it in lower case.
pub fn has_minimized_switch(args: &[String]) -> bool {
    args.iter().any(|a| matches!(clean(a), "--minimized" | "-minimized" | "/minimized" | "-m"))
}

/// `argument.Trim().Trim('"')`.
fn clean(argument: &str) -> &str {
    argument.trim().trim_matches('"')
}

fn strip_prefix_ignore_case<'a>(token: &'a str, prefix: &str) -> Option<&'a str> {
    let head = token.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &token[prefix.len()..])
}

/// `Guid.TryParse`: hyphenated, braced, parenthesized or bare hex, surrounding whitespace ignored.
fn parse_guid(text: &str) -> Option<Uuid> {
    let text = text.trim();
    let inner = text
        .strip_prefix('(')
        .and_then(|t| t.strip_suffix(')'))
        .unwrap_or(text);
    Uuid::parse_str(inner).ok()
}

fn is_download_url(text: &str) -> bool {
    let Ok(url) = url::Url::parse(text) else { return false };
    matches!(url.scheme(), "http" | "https" | "ftp" | "ftps") && url.host_str().is_some_and(|h| !h.is_empty())
}

#[cfg(test)]
mod tests {
    //! Port of `tests/QuickByte.UI.Tests/CommandLineTests.cs`, plus the
    //! mutex-and-pipe round trip under names of the tests' own.

    use std::sync::atomic::{AtomicUsize, Ordering};

    use quickbyte_core::events::NoopDispatcher;

    use super::*;

    const QUEUE_ID: &str = "6f9619ff-8b86-d011-b42d-00c04fc964ff";

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn queue_id() -> Uuid {
        Uuid::parse_str(QUEUE_ID).unwrap()
    }

    // ------------------------------------------------------------- URLs --

    #[test]
    fn every_scheme_the_add_dialog_accepts_is_recognised_on_the_command_line() {
        for url in ["http://example.com/file.zip", "https://example.com/file.zip", "ftp://example.com/file.zip", "ftps://example.com/file.zip"] {
            assert_eq!(find_url(&args(&[url])).as_deref(), Some(url));
        }
    }

    #[test]
    fn a_url_is_found_past_the_switches_around_it() {
        assert_eq!(
            find_url(&args(&["--minimized", "https://example.com/file.zip", "--run-queue"])).as_deref(),
            Some("https://example.com/file.zip")
        );
    }

    #[test]
    fn the_first_url_wins() {
        assert_eq!(
            find_url(&args(&["https://first.example/a.zip", "https://second.example/b.zip"])).as_deref(),
            Some("https://first.example/a.zip")
        );
    }

    #[test]
    fn the_shell_may_quote_or_pad_a_link_and_it_is_still_a_link() {
        for argument in ["\"https://example.com/file.zip\"", "  https://example.com/file.zip  "] {
            assert_eq!(find_url(&args(&[argument])).as_deref(), Some("https://example.com/file.zip"));
        }
    }

    #[test]
    fn something_that_is_not_a_link_is_not_offered_as_one() {
        for argument in ["example.com/file.zip", "--minimized", ""] {
            assert_eq!(find_url(&args(&[argument])), None, "{argument}");
        }
    }

    #[test]
    fn a_local_path_is_not_a_download() {
        assert_eq!(find_url(&args(&[r"C:\Users\someone\Downloads\file.zip"])), None);
        assert_eq!(find_url(&args(&["file:///C:/Windows/notepad.exe"])), None);
    }

    // ----------------------------------------------------------- queues --

    #[test]
    fn the_agents_switch_form_is_understood() {
        assert_eq!(find_queue_id(&args(&["--run-queue", QUEUE_ID])), Some(queue_id()));
    }

    #[test]
    fn the_payload_form_that_crosses_the_pipe_is_understood() {
        assert_eq!(find_queue_id(&args(&[&format!("run-queue:{QUEUE_ID}")])), Some(queue_id()));
    }

    #[test]
    fn the_switch_is_matched_whatever_its_case() {
        for spelling in ["--RUN-QUEUE", "--Run-Queue"] {
            assert_eq!(find_queue_id(&args(&[spelling, QUEUE_ID])), Some(queue_id()));
        }
    }

    #[test]
    fn a_quoted_switch_and_id_still_name_a_queue() {
        assert_eq!(find_queue_id(&args(&["\"--run-queue\"", &format!("\"{QUEUE_ID}\"")])), Some(queue_id()));
    }

    #[test]
    fn a_switch_with_nothing_after_it_names_no_queue() {
        assert_eq!(find_queue_id(&args(&["--run-queue"])), None);
    }

    #[test]
    fn a_switch_followed_by_something_that_is_not_an_id_names_no_queue() {
        assert_eq!(find_queue_id(&args(&["--run-queue", "nightly"])), None);
    }

    #[test]
    fn an_ordinary_launch_names_no_queue() {
        assert_eq!(find_queue_id(&args(&["https://example.com/file.zip", "--minimized"])), None);
    }

    // --------------------------------------------------------- hand-off --

    #[test]
    fn a_queue_is_what_gets_handed_over_when_both_are_present() {
        let payload = build_handoff_payload(&args(&["--run-queue", QUEUE_ID, "https://example.com/file.zip"]));
        assert_eq!(payload, format!("run-queue:{QUEUE_ID}"));
    }

    #[test]
    fn what_is_handed_over_is_what_the_other_side_reads_back() {
        let payload = build_handoff_payload(&args(&["--run-queue", QUEUE_ID]));
        assert_eq!(find_queue_id(&[payload]), Some(queue_id()));
    }

    #[test]
    fn a_link_is_handed_over_when_there_is_no_queue() {
        assert_eq!(build_handoff_payload(&args(&["--minimized", "https://example.com/file.zip"])), "https://example.com/file.zip");
    }

    #[test]
    fn a_launch_with_nothing_to_say_hands_over_nothing() {
        assert_eq!(build_handoff_payload(&[]), "");
        assert_eq!(build_handoff_payload(&args(&["--minimized"])), "");
    }

    #[test]
    fn the_queue_id_is_handed_over_in_the_form_it_is_parsed_in() {
        let payload = build_handoff_payload(&args(&["--run-queue", &format!("{{{QUEUE_ID}}}")]));
        assert_eq!(payload, format!("run-queue:{QUEUE_ID}"));
        assert!(!payload.contains('{'));
    }

    // -------------------------------------------------- quiet launches --

    #[test]
    fn every_spelling_of_the_minimized_switch_starts_quietly() {
        for argument in ["--minimized", "-minimized", "/minimized", "-m", "\"--minimized\"", "  -m  "] {
            assert!(has_minimized_switch(&args(&[argument])), "{argument}");
        }
    }

    #[test]
    fn anything_else_opens_a_window() {
        for argument in ["--min", "m", "--minimize", "https://example.com/file.zip"] {
            assert!(!has_minimized_switch(&args(&[argument])), "{argument}");
        }
    }

    #[test]
    fn the_minimized_switch_is_matched_case_sensitively() {
        assert!(!has_minimized_switch(&args(&["--Minimized"])));
    }

    // ----------------------------------------------- the real thing --

    #[test]
    fn the_names_are_the_csharp_builds() {
        assert_eq!(MUTEX_NAME, r"Local\QuickByte.SingleInstance");
        assert_eq!(pipe_name(), format!(r"\\.\pipe\QuickByte.SingleInstance.{}", user_name()));
        assert_eq!(RUN_QUEUE_SWITCH, "--run-queue");
        assert_eq!(QUEUE_PAYLOAD_PREFIX, "run-queue:");
    }

    struct CountingWaker(AtomicUsize);
    impl Dispatcher for CountingWaker {
        fn notify(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn scratch_names() -> (String, String) {
        let id = Uuid::new_v4().simple();
        (format!(r"Local\QuickByte.Shell.Tests.{id}"), format!(r"\\.\pipe\QuickByte.Shell.Tests.{id}"))
    }

    #[test]
    fn the_second_copy_loses_and_its_hand_off_reaches_the_first() {
        let (mutex, pipe) = scratch_names();
        let waker = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let first = SingleInstance::acquire_named(&mutex, &pipe, waker.clone()).expect("the first copy wins");

        assert!(SingleInstance::acquire_named(&mutex, &pipe, Arc::new(NoopDispatcher)).is_none());
        assert!(crate::win::mutex_exists(&mutex));

        let payloads = first.payloads();
        assert!(send_to(&pipe, "https://example.com/file.zip", HANDOFF_TIMEOUT));
        assert_eq!(payloads.recv_timeout(Duration::from_secs(5)).unwrap(), "https://example.com/file.zip");

        // A launch with nothing to say still gets through, as "".
        assert!(send_to(&pipe, "", HANDOFF_TIMEOUT));
        assert_eq!(payloads.recv_timeout(Duration::from_secs(5)).unwrap(), "");

        // Padding is trimmed, as the C# reader trimmed it.
        assert!(send_to(&pipe, &format!("  run-queue:{QUEUE_ID}\r\n"), HANDOFF_TIMEOUT));
        assert_eq!(payloads.recv_timeout(Duration::from_secs(5)).unwrap(), format!("run-queue:{QUEUE_ID}"));
        assert!(waker.0.load(Ordering::SeqCst) >= 3);

        drop(first);
        assert!(!crate::win::mutex_exists(&mutex));
        // The listener is gone with it: nobody answers any more.
        assert!(!send_to(&pipe, "late", Duration::from_millis(200)));
        // And the name can be taken again.
        assert!(SingleInstance::acquire_named(&mutex, &pipe, Arc::new(NoopDispatcher)).is_some());
    }

    #[test]
    fn a_hand_off_with_nobody_listening_fails_after_the_timeout() {
        let (_, pipe) = scratch_names();
        let started = Instant::now();
        assert!(!send_to(&pipe, "x", Duration::from_millis(150)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

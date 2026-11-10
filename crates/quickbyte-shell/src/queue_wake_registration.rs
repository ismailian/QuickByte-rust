//! Port of `src/QuickByte.UI/QueueWakeRegistration.cs` — the Task Scheduler task
//! that wakes the computer for a queue.
//!
//! Wakes a sleeping computer for a scheduled queue that asks for it, with one
//! Task Scheduler task per user whose triggers are the queues' start times and
//! whose `WakeToRun` is what sets the wake timer.
//!
//! The scheduler agent ([`crate::queue_agent_registration`]) covers QuickByte
//! being closed; it cannot cover the machine being asleep, because nothing runs
//! then. A wake timer is the one mechanism Windows offers for that, and Task
//! Scheduler is the one place a program without administrator rights can set a
//! persistent one — checked, not assumed: a standard user can register a task
//! with `WakeToRun` in the root folder, for themselves.
//!
//! The task runs as the signed-in user, only while they are signed in
//! (`InteractiveToken`), with least privilege: a download queue means nothing
//! outside the user's session. Its action is the agent with `--once`, which
//! starts whatever is due and exits; QuickByte then keeps the machine awake
//! while it downloads ([`crate::keep_awake`]).
//!
//! Registered through `schtasks.exe` by full path, with an XML definition built
//! here — no COM. Best-effort and silent, like both Run-key registrars: policy
//! can refuse it, and the schedule still runs whenever the machine happens to be
//! awake.

use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use quickbyte_core::enums::ScheduleDays;
use quickbyte_core::models::DownloadQueue;
#[cfg(test)]
use quickbyte_core::serde_compat::TimeSpan;

use crate::win::{hidden_command, process_path, system_directory, user_name};

const TASK_NAMESPACE: &str = "http://schemas.microsoft.com/windows/2004/02/mit/task";

/// A Monday, long past: triggers repeat weekly from it, and a fixed date keeps the definition comparable.
const ANCHOR: (i32, u32, u32) = (2020, 1, 6);

/// `QuickByte Queue Wake (<UserName>)`. One per user — task names are
/// machine-wide, and one user cannot replace another's task.
pub fn task_name() -> String {
    format!("QuickByte Queue Wake ({})", user_name())
}

/// Creates, updates or deletes the wake task on a background thread (serialized,
/// and skipped when the definition has not changed since the last apply).
///
/// Brings the task in line with the queues, off the UI thread: registered with
/// their wake times when any queue wants waking, deleted when none does, and
/// left alone when nothing changed since the last sync.
pub fn sync(queues: &[DownloadQueue]) {
    if quickbyte_core::helpers::app_paths::sandbox().is_some() {
        return; // a developer sandbox creates no scheduled task
    }
    let wakes = wake_times_for(queues);
    let _ = std::thread::Builder::new().name("qb-wake-registration".into()).spawn(move || {
        // The gate: one apply at a time, in the order the syncs were asked for
        // closely enough — each one applies the queues as they were when asked.
        let mut registrar = registrar().lock();
        let action = action_for();
        let sid = current_user_sid();
        let name = task_name();
        // Best-effort — see the module comment.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registrar.apply(&wakes, action.as_ref(), &name, &sid);
        }));
    });
}

fn registrar() -> &'static Mutex<WakeRegistrar> {
    static REGISTRAR: OnceLock<Mutex<WakeRegistrar>> = OnceLock::new();
    REGISTRAR.get_or_init(|| Mutex::new(WakeRegistrar::new(Box::new(run_schtasks_process))))
}

// ---------------------------------------------------------------------------
// WakeTimes — Core's, so the wake task and the schedule arithmetic can never
// disagree about which moments need waking.
// ---------------------------------------------------------------------------

pub(crate) use quickbyte_core::helpers::wake_times::{MAX_TRIGGERS, WakeTime};

pub(crate) fn wake_times_for(queues: &[DownloadQueue]) -> Vec<WakeTime> {
    quickbyte_core::helpers::wake_times::for_queues(queues)
}

// ---------------------------------------------------------------------------
// Applying
// ---------------------------------------------------------------------------

/// Runs schtasks.exe with the given arguments and returns its exit code.
/// Replaced in the tests, which must not register real wake timers.
pub(crate) type SchtasksRunner = Box<dyn FnMut(&[String]) -> i32 + Send>;

pub(crate) struct WakeRegistrar {
    run_schtasks: SchtasksRunner,
    /// The definition registered last in this session; `Some("")` once deleted,
    /// `None` before the first sync.
    applied: Option<String>,
}

impl WakeRegistrar {
    pub(crate) fn new(run_schtasks: SchtasksRunner) -> Self {
        Self { run_schtasks, applied: None }
    }

    /// One sync, already off the UI thread. `action` is what the task starts
    /// ([`action_for`]); `None` means there is nothing to run, so no wake is set.
    pub(crate) fn apply(&mut self, wakes: &[WakeTime], action: Option<&(String, String)>, task_name: &str, user_sid: &str) {
        let Some((command, arguments)) = action.filter(|_| !wakes.is_empty()) else {
            if self.applied.as_deref() == Some("") {
                return;
            }
            // Deleting a task that is not there fails; that is the state wanted.
            (self.run_schtasks)(&args(&["/Delete", "/TN", task_name, "/F"]));
            self.applied = Some(String::new());
            return;
        };

        let xml = build_definition(wakes, command, arguments, user_sid);
        if self.applied.as_deref() == Some(xml.as_str()) {
            return;
        }

        let path = std::env::temp_dir().join(format!("quickbyte-wake-{}.xml", uuid::Uuid::new_v4().simple()));
        // schtasks reads the definition as UTF-16, which is what the XML declares.
        if std::fs::write(&path, utf16_with_bom(&xml)).is_ok() {
            let path_text = path.display().to_string();
            if (self.run_schtasks)(&args(&["/Create", "/TN", task_name, "/XML", &path_text, "/F"])) == 0 {
                self.applied = Some(xml);
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// `Encoding.Unicode` as `File.WriteAllText` writes it: UTF-16 LE with a BOM.
fn utf16_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// What the task starts: the scheduler agent for one evaluation when it ships
/// beside the app, else QuickByte itself, minimized, whose own timer starts the
/// queue. `None` when neither can be found — a wake with nothing to run is not set.
pub(crate) fn action_for() -> Option<(String, String)> {
    if let Some(agent) = crate::queue_agent_registration::executable_path() {
        return Some((agent.display().to_string(), "--once".into()));
    }
    let app = process_path()?;
    app.is_file().then(|| (app.display().to_string(), "--minimized".into()))
}

/// The task's XML, as `XDeclaration + Environment.NewLine + XElement.ToString()`
/// produced it: two-space indentation, CRLF line breaks, the namespace declared
/// on the root after its `version` attribute, `<Monday />` for an empty element.
pub(crate) fn build_definition(wakes: &[WakeTime], command: &str, arguments: &str, user_sid: &str) -> String {
    let anchor = chrono::NaiveDate::from_ymd_opt(ANCHOR.0, ANCHOR.1, ANCHOR.2).expect("valid anchor");
    let midnight = anchor.and_hms_opt(0, 0, 0).expect("midnight");

    let triggers: Vec<Node> = wakes
        .iter()
        .take(MAX_TRIGGERS)
        .map(|wake| {
            let start = midnight + wake.at.to_delta();
            Node::el(
                "CalendarTrigger",
                vec![
                    Node::text("StartBoundary", &start.format("%Y-%m-%dT%H:%M:%S").to_string()),
                    Node::text("Enabled", "true"),
                    Node::el(
                        "ScheduleByWeek",
                        vec![
                            Node::el("DaysOfWeek", days_of(wake.days).into_iter().map(Node::empty).collect()),
                            Node::text("WeeksInterval", "1"),
                        ],
                    ),
                ],
            )
        })
        .collect();

    let working_directory = Path::new(command).parent().map(|p| p.display().to_string()).unwrap_or_default();

    let task = Node::el(
        "Task",
        vec![
            Node::el(
                "RegistrationInfo",
                vec![
                    Node::text("Author", "QuickByte"),
                    Node::text(
                        "Description",
                        "Wakes the computer when a QuickByte download queue is scheduled to start. \
                         Managed by QuickByte: untick \"Wake the computer\" on the queue to remove it.",
                    ),
                ],
            ),
            Node::el("Triggers", triggers),
            Node::el(
                "Principals",
                vec![
                    Node::el(
                        "Principal",
                        vec![
                            Node::text("UserId", user_sid),
                            Node::text("LogonType", "InteractiveToken"),
                            Node::text("RunLevel", "LeastPrivilege"),
                        ],
                    )
                    .attr("id", "Author"),
                ],
            ),
            Node::el(
                "Settings",
                vec![
                    Node::text("MultipleInstancesPolicy", "IgnoreNew"),
                    Node::text("DisallowStartIfOnBatteries", "false"),
                    Node::text("StopIfGoingOnBatteries", "false"),
                    Node::text("AllowHardTerminate", "true"),
                    Node::text("StartWhenAvailable", "false"),
                    Node::text("RunOnlyIfNetworkAvailable", "false"),
                    Node::text("AllowStartOnDemand", "true"),
                    Node::text("Enabled", "true"),
                    Node::text("Hidden", "false"),
                    Node::text("RunOnlyIfIdle", "false"),
                    Node::text("WakeToRun", "true"),
                    Node::text("ExecutionTimeLimit", "PT5M"),
                    Node::text("Priority", "7"),
                ],
            ),
            Node::el(
                "Actions",
                vec![Node::el(
                    "Exec",
                    vec![
                        Node::text("Command", command),
                        Node::text("Arguments", arguments),
                        Node::text("WorkingDirectory", &working_directory),
                    ],
                )],
            )
            .attr("Context", "Author"),
        ],
    )
    .attr("version", "1.2")
    .attr("xmlns", TASK_NAMESPACE);

    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n");
    task.write(&mut out, 0);
    out
}

fn days_of(days: ScheduleDays) -> Vec<&'static str> {
    use chrono::Weekday::*;
    [(Sun, "Sunday"), (Mon, "Monday"), (Tue, "Tuesday"), (Wed, "Wednesday"), (Thu, "Thursday"), (Fri, "Friday"), (Sat, "Saturday")]
        .into_iter()
        .filter(|(day, _)| days.contains(ScheduleDays::from_weekday(*day)))
        .map(|(_, name)| name)
        .collect()
}

/// Just enough of `XElement` to write the task definition the way LINQ to XML did.
struct Node {
    name: &'static str,
    attributes: Vec<(&'static str, String)>,
    content: Content,
}

enum Content {
    /// `new XElement(name)` — written as `<name />`.
    Empty,
    /// `new XElement(name, "text")` — `<name>text</name>`, even for "".
    Text(String),
    Children(Vec<Node>),
}

impl Node {
    fn el(name: &'static str, children: Vec<Node>) -> Node {
        let content = if children.is_empty() { Content::Empty } else { Content::Children(children) };
        Node { name, attributes: Vec::new(), content }
    }
    fn text(name: &'static str, text: &str) -> Node {
        Node { name, attributes: Vec::new(), content: Content::Text(text.to_string()) }
    }
    fn empty(name: &'static str) -> Node {
        Node { name, attributes: Vec::new(), content: Content::Empty }
    }
    fn attr(mut self, name: &'static str, value: &str) -> Node {
        self.attributes.push((name, value.to_string()));
        self
    }

    fn write(&self, out: &mut String, depth: usize) {
        let indent = "  ".repeat(depth);
        out.push_str(&indent);
        out.push('<');
        out.push_str(self.name);
        for (name, value) in &self.attributes {
            out.push(' ');
            out.push_str(name);
            out.push_str("=\"");
            escape(out, value, true);
            out.push('"');
        }
        match &self.content {
            Content::Empty => out.push_str(" />"),
            Content::Text(text) => {
                out.push('>');
                escape(out, text, false);
                out.push_str("</");
                out.push_str(self.name);
                out.push('>');
            }
            Content::Children(children) => {
                out.push('>');
                for child in children {
                    out.push_str("\r\n");
                    child.write(out, depth + 1);
                }
                out.push_str("\r\n");
                out.push_str(&indent);
                out.push_str("</");
                out.push_str(self.name);
                out.push('>');
            }
        }
    }
}

fn escape(out: &mut String, text: &str, attribute: bool) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

/// The signed-in user's SID (`WindowsIdentity.GetCurrent().User`), falling back
/// to the user name as the C# code fell back to `identity.Name`.
fn current_user_sid() -> String {
    sid_string().unwrap_or_else(user_name)
}

fn sid_string() -> Option<String> {
    use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::PWSTR;

    // SAFETY: standard token query; every buffer is sized by the first call and
    // the string from ConvertSidToStringSidW is freed with LocalFree.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let token = crate::win::OwnedHandle(token);
        let mut needed = 0u32;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            return None;
        }
        // u64 storage keeps TOKEN_USER suitably aligned.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        GetTokenInformation(token.0, TokenUser, Some(buffer.as_mut_ptr().cast()), needed, &mut needed).ok()?;
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut text = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut text).ok()?;
        let sid = text.to_string().ok();
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
        sid
    }
}

/// By full path, never by name: a schtasks.exe earlier on PATH or in the working
/// folder would be handed the task definition instead.
fn run_schtasks_process(arguments: &[String]) -> i32 {
    use std::process::Stdio;
    let schtasks = system_directory().join("schtasks.exe");
    let mut command = hidden_command(&schtasks);
    command.args(arguments).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else { return -1 };

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code().unwrap_or(-1),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                return -1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Port of `tests/QuickByte.UI.Tests/QueueWakeRegistrationTests.cs`.
    //! schtasks itself is replaced for every test — a test run that registered
    //! real wake timers would switch the machine on in the night.

    use std::sync::Arc;

    use quickbyte_core::models::QueueSchedule;

    use super::*;
    use crate::queue_agent_registration::AGENT_FILE_NAME;
    use crate::win::test_support::FakeHost;

    const NS: &str = TASK_NAMESPACE;

    fn two_wakes() -> Vec<WakeTime> {
        vec![
            WakeTime { days: ScheduleDays::WEEKDAYS, at: TimeSpan::from_seconds(2 * 3600 + 30 * 60) },
            WakeTime { days: ScheduleDays::SUNDAY | ScheduleDays::SATURDAY, at: TimeSpan::from_seconds(23 * 3600 + 5 * 60 + 9) },
        ]
    }

    type Calls = Arc<Mutex<Vec<(Vec<String>, Option<Vec<u8>>)>>>;

    /// A registrar whose schtasks records each call (and the definition file's
    /// bytes, read while it still exists) and answers with `exit_code`.
    fn recording(exit_code: Arc<Mutex<i32>>) -> (WakeRegistrar, Calls) {
        let calls: Calls = Arc::default();
        let sink = calls.clone();
        let registrar = WakeRegistrar::new(Box::new(move |arguments: &[String]| {
            let xml = arguments.iter().position(|a| a == "/XML").and_then(|i| std::fs::read(&arguments[i + 1]).ok());
            sink.lock().push((arguments.to_vec(), xml));
            *exit_code.lock()
        }));
        (registrar, calls)
    }

    fn action() -> (String, String) {
        (r"C:\Apps\QuickByte.Agent.exe".into(), "--once".into())
    }

    fn decode(bytes: &[u8]) -> String {
        let units: Vec<u16> = bytes[2..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16(&units).unwrap()
    }

    fn parse(xml: &str) -> xml_lookup::Doc {
        xml_lookup::Doc::new(xml)
    }

    /// A tiny element lookup over the generated XML — enough for the assertions,
    /// without pulling an XML parser into the shell crate's dependencies.
    mod xml_lookup {
        pub struct Doc(String);
        impl Doc {
            pub fn new(xml: &str) -> Self {
                Doc(xml.to_string())
            }
            /// The text of every `<name>text</name>`.
            pub fn values(&self, name: &str) -> Vec<String> {
                let open = format!("<{name}>");
                let close = format!("</{name}>");
                let mut out = Vec::new();
                let mut rest = self.0.as_str();
                while let Some(start) = rest.find(&open) {
                    let after = &rest[start + open.len()..];
                    let end = after.find(&close).expect("closed");
                    out.push(unescape(&after[..end]));
                    rest = &after[end..];
                }
                out
            }
            pub fn single(&self, name: &str) -> String {
                let v = self.values(name);
                assert_eq!(v.len(), 1, "{name}: {v:?}");
                v.into_iter().next().unwrap()
            }
            pub fn text(&self) -> &str {
                &self.0
            }
        }
        fn unescape(s: &str) -> String {
            s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&")
        }
    }

    #[test]
    fn the_task_wakes_the_computer_as_the_signed_in_user_with_least_privilege() {
        let task = parse(&build_definition(&two_wakes(), r"C:\Apps\QuickByte.Agent.exe", "--once", "S-1-5-21-1-2-3-1001"));

        assert_eq!(task.single("WakeToRun"), "true");
        assert_eq!(task.single("UserId"), "S-1-5-21-1-2-3-1001");
        assert_eq!(task.single("LogonType"), "InteractiveToken");
        assert_eq!(task.single("RunLevel"), "LeastPrivilege");

        // A wake on battery is still a wake the user asked for.
        assert_eq!(task.single("DisallowStartIfOnBatteries"), "false");

        assert_eq!(task.single("Command"), r"C:\Apps\QuickByte.Agent.exe");
        assert_eq!(task.single("Arguments"), "--once");
        assert_eq!(task.single("WorkingDirectory"), r"C:\Apps");
    }

    #[test]
    fn the_definition_has_the_shape_linq_to_xml_wrote() {
        let xml = build_definition(&two_wakes()[..1], r"C:\Apps\QuickByte.Agent.exe", "--once", "S-1");

        assert!(xml.starts_with(&format!("<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n<Task version=\"1.2\" xmlns=\"{NS}\">\r\n  <RegistrationInfo>\r\n    <Author>QuickByte</Author>")), "{xml}");
        assert!(xml.contains("\r\n        <DaysOfWeek>\r\n          <Monday />\r\n"), "{xml}");
        assert!(xml.contains("<Principal id=\"Author\">"));
        assert!(xml.contains("<Actions Context=\"Author\">"));
        assert!(xml.ends_with("\r\n  </Actions>\r\n</Task>"));
    }

    #[test]
    fn each_wake_is_a_weekly_trigger_on_its_days_at_its_time() {
        let task = parse(&build_definition(&two_wakes(), r"C:\Apps\QuickByte.Agent.exe", "--once", "S-1"));

        assert_eq!(task.values("StartBoundary"), vec!["2020-01-06T02:30:00", "2020-01-06T23:05:09"]);
        let triggers: Vec<&str> = task.text().split("<CalendarTrigger>").skip(1).collect();
        assert_eq!(triggers.len(), 2);
        let days = |t: &str| -> Vec<String> {
            let block = &t[t.find("<DaysOfWeek>").unwrap()..t.find("</DaysOfWeek>").unwrap()];
            block.split('<').filter_map(|s| s.trim_end().strip_suffix(" />")).map(String::from).collect()
        };
        assert_eq!(days(triggers[0]), vec!["Monday", "Tuesday", "Wednesday", "Thursday", "Friday"]);
        assert_eq!(days(triggers[1]), vec!["Sunday", "Saturday"]);
        assert_eq!(task.values("WeeksInterval"), vec!["1", "1"]);
    }

    #[test]
    fn a_path_with_characters_that_mean_something_in_xml_stays_the_path() {
        let odd = r"C:\Tools & <Things>\QuickByte.Agent.exe";

        let xml = build_definition(&two_wakes(), odd, "--once", "S-1");

        assert!(xml.contains(r"<Command>C:\Tools &amp; &lt;Things&gt;\QuickByte.Agent.exe</Command>"));
        assert_eq!(parse(&xml).single("Command"), odd);
    }

    #[test]
    fn wake_times_register_the_task_once_as_utf16_and_leave_no_file_behind() {
        let (mut registrar, calls) = recording(Arc::new(Mutex::new(0)));

        registrar.apply(&two_wakes(), Some(&action()), "QuickByte Queue Wake (someone)", "S-1");
        registrar.apply(&two_wakes(), Some(&action()), "QuickByte Queue Wake (someone)", "S-1");

        let calls = calls.lock();
        assert_eq!(calls.len(), 1);
        let (arguments, definition) = &calls[0];
        assert_eq!(&arguments[..4], &["/Create", "/TN", "QuickByte Queue Wake (someone)", "/XML"]);
        assert_eq!(arguments.last().unwrap(), "/F");
        assert!(decode(definition.as_ref().unwrap()).contains("<WakeToRun>true</WakeToRun>"));
        assert!(!Path::new(&arguments[4]).exists());
    }

    #[test]
    fn the_definition_is_written_as_the_utf16_its_declaration_names() {
        let (mut registrar, calls) = recording(Arc::new(Mutex::new(0)));

        registrar.apply(&two_wakes(), Some(&action()), "t", "S-1");

        let bytes = calls.lock()[0].1.clone().unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        assert!(decode(&bytes).starts_with("<?xml version=\"1.0\" encoding=\"UTF-16\"?>"));
    }

    #[test]
    fn a_changed_time_registers_again_and_a_refused_registration_is_tried_again() {
        let exit_code = Arc::new(Mutex::new(1));
        let (mut registrar, calls) = recording(exit_code.clone());

        registrar.apply(&two_wakes(), Some(&action()), "t", "S-1");
        *exit_code.lock() = 0;
        registrar.apply(&two_wakes(), Some(&action()), "t", "S-1");
        registrar.apply(&[WakeTime { days: ScheduleDays::MONDAY, at: TimeSpan::from_hours(4) }], Some(&action()), "t", "S-1");

        let calls = calls.lock();
        assert_eq!(calls.len(), 3);
        assert!(calls.iter().all(|(a, _)| a[0] == "/Create"));
    }

    #[test]
    fn no_queue_wanting_a_wake_deletes_the_task_once() {
        let (mut registrar, calls) = recording(Arc::new(Mutex::new(0)));

        registrar.apply(&two_wakes(), Some(&action()), "t", "S-1");
        registrar.apply(&[], Some(&action()), "t", "S-1");
        registrar.apply(&[], Some(&action()), "t", "S-1");

        let calls = calls.lock();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, vec!["/Delete", "/TN", "t", "/F"]);
    }

    #[test]
    fn nothing_to_run_is_no_wake_either() {
        let (mut registrar, calls) = recording(Arc::new(Mutex::new(0)));

        registrar.apply(&two_wakes(), None, "t", "S-1");

        assert_eq!(calls.lock()[0].0[0], "/Delete");
    }

    #[test]
    fn the_task_runs_the_agent_for_one_look_when_it_ships_beside_the_app() {
        {
            let host = FakeHost::with_agent();
            assert_eq!(action_for(), Some((host.file(AGENT_FILE_NAME).display().to_string(), "--once".into())));
        }
        // Without one, QuickByte itself — minimized, its own timer starts the queue.
        let host = FakeHost::without_agent();
        assert_eq!(action_for(), Some((host.exe.display().to_string(), "--minimized".into())));
    }

    #[test]
    fn the_task_is_named_for_the_user_so_two_accounts_never_share_one() {
        let name = task_name();
        assert!(name.starts_with("QuickByte Queue Wake ("));
        assert!(name.ends_with(&format!("({})", user_name())));
        assert!(!name.contains('\\'));
    }

    #[test]
    fn the_current_user_has_a_sid() {
        assert!(current_user_sid().starts_with("S-1-5-"));
    }

    // ------------------------------------------------------- WakeTimes --

    fn queue(enabled: bool, wake: bool, days: ScheduleDays, start: TimeSpan) -> DownloadQueue {
        DownloadQueue {
            schedule: QueueSchedule { enabled, wake_computer: wake, days, start_time: start, ..QueueSchedule::default() },
            ..DownloadQueue::default()
        }
    }

    #[test]
    fn only_enabled_schedules_that_asked_for_a_wake_are_woken_for() {
        let queues = vec![
            queue(true, true, ScheduleDays::MONDAY, TimeSpan::from_hours(2)),
            queue(true, false, ScheduleDays::MONDAY, TimeSpan::from_hours(3)),
            queue(false, true, ScheduleDays::MONDAY, TimeSpan::from_hours(4)),
            queue(true, true, ScheduleDays::NONE, TimeSpan::from_hours(5)),
        ];

        assert_eq!(wake_times_for(&queues), vec![WakeTime { days: ScheduleDays::MONDAY, at: TimeSpan::from_hours(2) }]);
    }

    #[test]
    fn queues_starting_at_the_same_second_share_one_wake_on_all_their_days() {
        let queues = vec![
            queue(true, true, ScheduleDays::SATURDAY, TimeSpan::from_hours(23)),
            queue(true, true, ScheduleDays::MONDAY, TimeSpan::from_hours(2)),
            queue(true, true, ScheduleDays::FRIDAY, TimeSpan::from_secs_f64(2.0 * 3600.0 + 0.4)),
        ];

        assert_eq!(
            wake_times_for(&queues),
            vec![
                WakeTime { days: ScheduleDays::MONDAY | ScheduleDays::FRIDAY, at: TimeSpan::from_hours(2) },
                WakeTime { days: ScheduleDays::SATURDAY, at: TimeSpan::from_hours(23) },
            ]
        );
    }

    #[test]
    fn an_impossible_time_is_read_as_midnight_and_the_triggers_are_capped() {
        let odd = vec![queue(true, true, ScheduleDays::MONDAY, TimeSpan::from_hours(30))];
        assert_eq!(wake_times_for(&odd)[0].at, TimeSpan::ZERO);

        let many: Vec<DownloadQueue> =
            (0..60).map(|m| queue(true, true, ScheduleDays::MONDAY, TimeSpan::from_minutes(m))).collect();
        assert_eq!(wake_times_for(&many).len(), MAX_TRIGGERS);
    }
}

//! Port of `CommandLine.cs`: reads `qb`'s arguments. Pure — the working
//! directory is a parameter — so the grammar is tested rather than discovered:
//! which switch takes a value, what a relative folder means, which combinations
//! make no sense.

use super::messages::{CommandRequest, CommandVerb};
use crate::models::is_path_fully_qualified;

/// What a command line asked for: a request to send, help, the version, or a usage mistake.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedCommandLine {
    pub request: Option<CommandRequest>,
    pub show_help: bool,
    pub show_version: bool,
    pub json: bool,
    pub error: Option<String>,
}

impl ParsedCommandLine {
    pub fn help() -> Self {
        Self { show_help: true, ..Default::default() }
    }
    pub fn mistake(error: impl Into<String>) -> Self {
        Self { error: Some(error.into()), ..Default::default() }
    }
    fn request(request: CommandRequest) -> Self {
        Self { request: Some(request), ..Default::default() }
    }
}

pub const USAGE: &str = "qb - control QuickByte from a prompt or a script.

Usage:
  qb add <url> [<url>...] [options]   Add downloads (QuickByte is started if needed)
      -o, --folder <folder>    Save into this folder (default: as Options says)
      -n, --name <file>        File name, for a single link
      -c, --connections <n>    Connections per download (1-32)
      -q, --queue <name>       Put them in this queue (created if missing) instead of starting them
          --paused             Add them without starting
          --checksum <hash>    SHA-256 (or SHA-1/MD5) the file must match
          --quality <q>        Streaming video: best (default), worst, or part of a label (\"720p\")
  qb list [--status active|paused|queued|completed|failed|all] [--json]
  qb pause <id|all>                   Pause a download (the first characters of its id are enough)
  qb resume <id|all>
  qb queues [--json]                  List the queues
  qb queue start <name>
  qb queue stop <name>
  qb status [--json]
  qb help | qb --version

Exit codes: 0 done, 1 QuickByte refused or failed, 2 bad command line, 3 QuickByte not running.";

const FILTERS: [&str; 6] = ["active", "paused", "queued", "completed", "failed", "all"];

pub fn parse<S: AsRef<str>>(args: &[S], working_directory: &str) -> ParsedCommandLine {
    let Some(first) = args.first().map(AsRef::as_ref) else {
        return ParsedCommandLine::help();
    };

    let verb = first.to_lowercase();
    if matches!(verb.as_str(), "help" | "-h" | "--help" | "/?") {
        return ParsedCommandLine::help();
    }
    if matches!(verb.as_str(), "version" | "--version" | "-v") {
        return ParsedCommandLine { show_version: true, ..Default::default() };
    }

    let mut rest: Vec<&str> = args[1..].iter().map(AsRef::as_ref).collect();
    let before = rest.len();
    rest.retain(|a| !a.eq_ignore_ascii_case("--json"));
    let json = rest.len() != before;

    let parsed = match verb.as_str() {
        "add" => parse_add(&rest, working_directory),
        "list" | "ls" => parse_list(&rest),
        "pause" => parse_target(CommandVerb::Pause, &rest),
        "resume" => parse_target(CommandVerb::Resume, &rest),
        "queues" => no_arguments(CommandVerb::Queues, &rest),
        "queue" => parse_queue(&rest),
        "status" => no_arguments(CommandVerb::Status, &rest),
        _ => ParsedCommandLine::mistake(format!("'{first}' is not a qb command. Run 'qb help' to see them.")),
    };
    ParsedCommandLine { json, ..parsed }
}

/// Whether a request needs QuickByte started when it is not running. Reading state does not.
pub fn starts_quickbyte(request: &CommandRequest) -> bool {
    matches!(request.verb, CommandVerb::Add | CommandVerb::QueueStart)
}

fn parse_add(args: &[&str], working_directory: &str) -> ParsedCommandLine {
    let mut request = CommandRequest::new(CommandVerb::Add);

    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if !arg.starts_with('-') {
            request.urls.push(arg.trim().trim_matches('"').to_string());
            i += 1;
            continue;
        }

        let name = arg.to_lowercase();
        if name == "--paused" {
            request.paused = true;
            i += 1;
            continue;
        }

        if !matches!(
            name.as_str(),
            "-o" | "--folder" | "-n" | "--name" | "-c" | "--connections" | "-q" | "--queue" | "--checksum" | "--quality"
        ) {
            return ParsedCommandLine::mistake(format!("'{arg}' is not an option of 'qb add'."));
        }

        let Some(value) = args.get(i + 1).filter(|v| !v.trim().is_empty()) else {
            return ParsedCommandLine::mistake(format!("'{arg}' needs a value."));
        };
        let value = value.trim();
        i += 2;

        match name.as_str() {
            "-o" | "--folder" => {
                // Relative to where qb was run, which only this side of the pipe knows.
                match full_path(&expand_environment_variables(value.trim_matches('"')), working_directory) {
                    Some(folder) => request.folder = Some(folder),
                    None => return ParsedCommandLine::mistake(format!("'{value}' is not a folder path.")),
                }
            }
            "-n" | "--name" => request.file_name = Some(value.trim_matches('"').to_string()),
            "-c" | "--connections" => {
                let connections = (value.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| value.parse::<i32>().ok())
                    .flatten()
                    .filter(|c| (1..=32).contains(c));
                match connections {
                    Some(c) => request.connections = Some(c),
                    None => return ParsedCommandLine::mistake("--connections takes a number from 1 to 32."),
                }
            }
            "-q" | "--queue" => request.queue = Some(value.trim_matches('"').to_string()),
            "--checksum" => request.checksum = Some(value.to_string()),
            _ => request.quality = Some(value.trim_matches('"').to_string()),
        }
    }

    if request.urls.is_empty() {
        return ParsedCommandLine::mistake("'qb add' needs at least one link.");
    }
    if request.file_name.is_some() && request.urls.len() > 1 {
        return ParsedCommandLine::mistake("--name names one file; give one link with it.");
    }
    if request.checksum.is_some() && request.urls.len() > 1 {
        return ParsedCommandLine::mistake("--checksum belongs to one file; give one link with it.");
    }
    if request.queue.is_some() && request.paused {
        return ParsedCommandLine::mistake("A download added to a queue waits for the queue already; --paused is not needed.");
    }
    ParsedCommandLine::request(request)
}

fn parse_list(args: &[&str]) -> ParsedCommandLine {
    let mut request = CommandRequest { verb: CommandVerb::List, filter: Some("all".into()), ..Default::default() };
    let mut i = 0;
    while i < args.len() {
        if !args[i].eq_ignore_ascii_case("--status") {
            return ParsedCommandLine::mistake(format!("'{}' is not an option of 'qb list'.", args[i]));
        }
        let Some(filter) = args.get(i + 1).map(|f| f.to_lowercase()).filter(|f| FILTERS.contains(&f.as_str())) else {
            return ParsedCommandLine::mistake(format!("--status takes one of: {}.", FILTERS.join(", ")));
        };
        request.filter = Some(filter);
        i += 2;
    }
    ParsedCommandLine::request(request)
}

fn parse_target(verb: CommandVerb, args: &[&str]) -> ParsedCommandLine {
    if args.len() != 1 {
        return ParsedCommandLine::mistake(format!("'qb {}' takes one download id, or 'all'.", verb.name().to_lowercase()));
    }
    ParsedCommandLine::request(CommandRequest { verb, target: Some(args[0].trim().to_string()), ..Default::default() })
}

fn parse_queue(args: &[&str]) -> ParsedCommandLine {
    if args.len() < 2 || !matches!(args[0].to_lowercase().as_str(), "start" | "stop") {
        return ParsedCommandLine::mistake("Use 'qb queue start <name>' or 'qb queue stop <name>'.");
    }
    let verb = if args[0].eq_ignore_ascii_case("start") { CommandVerb::QueueStart } else { CommandVerb::QueueStop };
    // A name with spaces may arrive as several arguments when it was not quoted.
    let queue = args[1..].join(" ").trim().trim_matches('"').to_string();
    ParsedCommandLine::request(CommandRequest { verb, queue: Some(queue), ..Default::default() })
}

fn no_arguments(verb: CommandVerb, args: &[&str]) -> ParsedCommandLine {
    if args.is_empty() {
        ParsedCommandLine::request(CommandRequest::new(verb))
    } else {
        ParsedCommandLine::mistake(format!("'qb {}' takes no arguments.", verb.name().to_lowercase()))
    }
}

/// `Environment.ExpandEnvironmentVariables`: each `%NAME%` that names a variable
/// becomes its value; anything else is left exactly as written.
pub fn expand_environment_variables(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('%') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('%') {
            Some(close) => {
                let name = &after[..close];
                match (!name.is_empty()).then(|| std::env::var(name).ok()).flatten() {
                    Some(value) => {
                        out.push_str(&value);
                        rest = &after[close + 1..];
                    }
                    None => {
                        // Not a variable: keep the text, and let the closing '%'
                        // start the next candidate.
                        out.push('%');
                        out.push_str(name);
                        rest = &after[close..];
                    }
                }
            }
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `Path.GetFullPath(path, basePath)` with Windows rules: a fully qualified path
/// is normalised, a rooted one (`\x`) takes the base's drive, anything else is
/// joined to the base; `.` and `..` are resolved and `/` becomes `\`. `None` for
/// what cannot be a path.
pub fn full_path(path: &str, base: &str) -> Option<String> {
    if path.contains('\0') || base.contains('\0') {
        return None;
    }
    let path = path.replace('/', "\\");
    let base = base.replace('/', "\\");

    let combined = if path.is_empty() {
        base.clone()
    } else if is_path_fully_qualified(&path) {
        path.clone()
    } else if path.starts_with('\\') {
        // Rooted on the current drive.
        format!("{}{}", root_of(&base)?.trim_end_matches('\\'), path)
    } else if path.len() >= 2 && path.as_bytes()[0].is_ascii_alphabetic() && path.as_bytes()[1] == b':' {
        // "D:folder": relative to that drive. Against the base when it is the same
        // drive; otherwise the drive's root is the best that can be said.
        if base.get(..2).is_some_and(|d| d.eq_ignore_ascii_case(&path[..2])) {
            format!("{}\\{}", base.trim_end_matches('\\'), &path[2..])
        } else {
            format!("{}\\{}", &path[..2], &path[2..])
        }
    } else {
        if !is_path_fully_qualified(&base) {
            return None;
        }
        format!("{}\\{}", base.trim_end_matches('\\'), path)
    };
    normalize(&combined)
}

/// `C:\` or `\\server\share\` of a fully qualified path.
fn root_of(path: &str) -> Option<String> {
    let b = path.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' {
        return Some(path[..3].to_string());
    }
    if let Some(unc) = path.strip_prefix("\\\\") {
        let mut parts = unc.split('\\');
        let server = parts.next().filter(|s| !s.is_empty())?;
        let share = parts.next().filter(|s| !s.is_empty())?;
        return Some(format!("\\\\{server}\\{share}\\"));
    }
    None
}

fn normalize(path: &str) -> Option<String> {
    let root = root_of(path)?;
    let trailing = path.len() > root.len() && path.ends_with('\\');
    let mut segments: Vec<&str> = Vec::new();
    for segment in path[root.len().min(path.len())..].split('\\') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    let mut full = root;
    full.push_str(&segments.join("\\"));
    if trailing && !segments.is_empty() {
        full.push('\\');
    }
    Some(full)
}

#[cfg(test)]
mod tests {
    //! A script depends on every one of these readings staying put, and a usage
    //! mistake has to be caught here — before anything is sent to QuickByte.

    use super::*;

    const CWD: &str = r"C:\Users\me\Videos";

    fn p(args: &[&str]) -> ParsedCommandLine {
        parse(args, CWD)
    }

    #[test]
    fn nothing_or_a_request_for_help_shows_the_usage() {
        assert!(p(&[]).show_help);
        for arg in ["help", "--help", "-h", "/?", "HELP"] {
            assert!(p(&[arg]).show_help, "{arg}");
        }
    }

    #[test]
    fn an_add_reads_its_links_and_every_option() {
        let parsed = p(&[
            "add",
            "https://example.com/a.iso",
            "-o",
            "Downloads",
            "-n",
            "disc.iso",
            "-c",
            "16",
            "--checksum",
            "sha256:ab",
            "--quality",
            "720p",
            "--json",
        ]);
        assert_eq!(parsed.error, None);
        assert!(parsed.json);
        let request = parsed.request.unwrap();
        assert_eq!(request.verb, CommandVerb::Add);
        assert_eq!(request.urls, ["https://example.com/a.iso"]);
        assert_eq!(request.folder.as_deref(), Some(r"C:\Users\me\Videos\Downloads"));
        assert_eq!(request.file_name.as_deref(), Some("disc.iso"));
        assert_eq!(request.connections, Some(16));
        assert_eq!(request.checksum.as_deref(), Some("sha256:ab"));
        assert_eq!(request.quality.as_deref(), Some("720p"));
        assert!(!request.paused);
    }

    #[test]
    fn a_folder_given_in_full_is_kept_and_environment_variables_are_expanded() {
        assert_eq!(p(&["add", "https://x/a", "--folder", r"D:\Iso"]).request.unwrap().folder.as_deref(), Some(r"D:\Iso"));

        if let Ok(root) = std::env::var("SystemRoot") {
            let expanded = p(&["add", "https://x/a", "--folder", r"%SystemRoot%\Temp"]).request.unwrap().folder.unwrap();
            assert_eq!(expanded, format!(r"{root}\Temp"));
        }
    }

    #[test]
    fn relative_folders_resolve_against_the_working_directory() {
        let folder = |f: &str| p(&["add", "https://x/a", "-o", f]).request.unwrap().folder.unwrap();
        assert_eq!(folder(r"..\Music"), r"C:\Users\me\Music");
        assert_eq!(folder("./a/b"), r"C:\Users\me\Videos\a\b");
        assert_eq!(folder(r"\Temp"), r"C:\Temp");
        assert_eq!(folder(r"D:\a\..\b\"), r"D:\b\");
        assert_eq!(folder(r#""D:\With Space""#), r"D:\With Space");
        assert_eq!(folder(r"\\nas\share\x\..\y"), r"\\nas\share\y");
    }

    #[test]
    fn unknown_variables_are_left_as_written() {
        assert_eq!(expand_environment_variables("%QB_SURELY_UNDEFINED%\\x"), "%QB_SURELY_UNDEFINED%\\x");
        assert_eq!(expand_environment_variables("100%"), "100%");
        assert_eq!(expand_environment_variables("%%"), "%%");
    }

    #[test]
    fn several_links_go_into_a_queue_or_are_left_paused() {
        let queued = p(&["add", "https://x/1", "https://x/2", "--queue", "Night"]).request.unwrap();
        assert_eq!(queued.urls.len(), 2);
        assert_eq!(queued.queue.as_deref(), Some("Night"));
        assert!(p(&["add", "https://x/1", "--paused"]).request.unwrap().paused);
    }

    #[test]
    fn a_usage_mistake_is_caught_before_anything_is_sent() {
        let cases: &[&[&str]] = &[
            &["add"],
            &["add", "--paused"],
            &["add", "https://x/1", "https://x/2", "--name", "a.iso"],
            &["add", "https://x/1", "https://x/2", "--checksum", "abc"],
            &["add", "https://x/1", "--connections", "0"],
            &["add", "https://x/1", "--connections", "64"],
            &["add", "https://x/1", "--connections", "many"],
            &["add", "https://x/1", "--connections", "+8"],
            &["add", "https://x/1", "--folder"],
            &["add", "https://x/1", "--folder", "  "],
            &["add", "https://x/1", "--speed", "5"],
            &["add", "https://x/1", "--queue", "Night", "--paused"],
            &["list", "--status", "sleeping"],
            &["list", "--status"],
            &["list", "extra"],
            &["pause"],
            &["pause", "a", "b"],
            &["queue", "rename", "Night"],
            &["queue", "start"],
            &["status", "now"],
            &["download", "https://x/1"],
        ];
        for args in cases {
            let parsed = p(args);
            assert!(parsed.error.is_some(), "{args:?}");
            assert!(parsed.request.is_none(), "{args:?}");
        }
        assert_eq!(p(&["download"]).error.as_deref(), Some("'download' is not a qb command. Run 'qb help' to see them."));
        assert_eq!(p(&["status", "now"]).error.as_deref(), Some("'qb status' takes no arguments."));
        assert_eq!(p(&["pause"]).error.as_deref(), Some("'qb pause' takes one download id, or 'all'."));
    }

    #[test]
    fn reading_commands_take_their_target_or_filter() {
        assert_eq!(p(&["list", "--status", "COMPLETED"]).request.unwrap().filter.as_deref(), Some("completed"));
        assert_eq!(p(&["ls"]).request.unwrap().filter.as_deref(), Some("all"));
        let pause = p(&["pause", "3f9c"]).request.unwrap();
        assert_eq!((pause.target.as_deref(), pause.verb), (Some("3f9c"), CommandVerb::Pause));
        assert_eq!(p(&["resume", "all"]).request.unwrap().target.as_deref(), Some("all"));
    }

    #[test]
    fn a_queue_name_with_spaces_survives_not_being_quoted() {
        let request = p(&["queue", "stop", "Late", "night", "films"]).request.unwrap();
        assert_eq!((request.verb, request.queue.as_deref()), (CommandVerb::QueueStop, Some("Late night films")));
    }

    #[test]
    fn only_commands_that_add_work_start_quickbyte() {
        assert!(starts_quickbyte(&p(&["add", "https://x/1"]).request.unwrap()));
        assert!(starts_quickbyte(&p(&["queue", "start", "Night"]).request.unwrap()));
        assert!(!starts_quickbyte(&p(&["list"]).request.unwrap()));
        assert!(!starts_quickbyte(&p(&["pause", "all"]).request.unwrap()));
        assert!(!starts_quickbyte(&p(&["status"]).request.unwrap()));
    }

    #[test]
    fn the_version_is_asked_for_in_the_usual_ways() {
        assert!(p(&["--version"]).show_version);
        assert!(p(&["version"]).show_version);
        assert!(p(&["-v"]).show_version);
    }

    #[test]
    fn json_is_taken_from_anywhere_on_the_line() {
        let parsed = p(&["status", "--JSON"]);
        assert!(parsed.json);
        assert_eq!(parsed.request.unwrap().verb, CommandVerb::Status);
    }
}

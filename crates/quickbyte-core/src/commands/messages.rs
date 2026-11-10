//! Port of `CommandMessages.cs`: the messages that cross the pipe between `qb`
//! and a running QuickByte, and that `qb --json` prints.
//!
//! **Wire format.** Written in camelCase, without the parts that are not set —
//! the shape `CommandFormatter.Json` gave `qb --json`, which scripts parse. Read
//! without regard to case, as the C# pipe's `PropertyNameCaseInsensitive` did:
//! every field also answers to its PascalCase name (what the C# pipe wrote) and
//! its lower-case one, so an old `qb.exe` and this build understand each other in
//! either direction. Enums are numbers, as System.Text.Json wrote them.

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::models::null_as_default;

/// The verbs `qb` sends to a running QuickByte.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum CommandVerb {
    #[default]
    Status = 0,
    Add = 1,
    List = 2,
    Pause = 3,
    Resume = 4,
    Queues = 5,
    QueueStart = 6,
    QueueStop = 7,
}

impl CommandVerb {
    /// The C# enum name (`QueueStart`), which `qb`'s messages lower-case.
    pub fn name(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::Add => "Add",
            Self::List => "List",
            Self::Pause => "Pause",
            Self::Resume => "Resume",
            Self::Queues => "Queues",
            Self::QueueStart => "QueueStart",
            Self::QueueStop => "QueueStop",
        }
    }
}

/// One command, as it crosses the pipe from `qb.exe` to QuickByte. Plain data,
/// already parsed and resolved on the command line's side — a relative folder is
/// made absolute where the working directory it is relative to still exists.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandRequest {
    #[serde(rename = "verb", alias = "Verb", alias = "VERB")]
    pub verb: CommandVerb,

    /// Add: the links.
    #[serde(rename = "urls", alias = "Urls", alias = "URLS", deserialize_with = "null_as_default")]
    pub urls: Vec<String>,

    /// Add: a folder to save into, absolute; `None` for the folder Options chooses.
    #[serde(rename = "folder", alias = "Folder", alias = "FOLDER", skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,

    /// Add: the file name, for a single link.
    #[serde(rename = "fileName", alias = "FileName", alias = "filename", alias = "FILENAME", skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,

    #[serde(rename = "connections", alias = "Connections", alias = "CONNECTIONS", skip_serializing_if = "Option::is_none")]
    pub connections: Option<i32>,

    /// Add: the queue to file the downloads into, created if missing. Queue start/stop: the queue.
    #[serde(rename = "queue", alias = "Queue", alias = "QUEUE", skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,

    /// Add: leave the downloads paused rather than starting them.
    #[serde(rename = "paused", alias = "Paused", alias = "PAUSED")]
    pub paused: bool,

    #[serde(rename = "checksum", alias = "Checksum", alias = "CHECKSUM", skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,

    /// Add: for a streaming video, "best", "worst", or text a quality's label contains.
    #[serde(rename = "quality", alias = "Quality", alias = "QUALITY", skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,

    /// Pause/resume: a download id (or the start of one), or "all".
    #[serde(rename = "target", alias = "Target", alias = "TARGET", skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,

    /// List: active, paused, queued, completed, failed or all.
    #[serde(rename = "filter", alias = "Filter", alias = "FILTER", skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
}

impl CommandRequest {
    pub fn new(verb: CommandVerb) -> Self {
        Self { verb, ..Default::default() }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandResponse {
    #[serde(rename = "ok", alias = "Ok", alias = "OK")]
    pub ok: bool,

    /// What went wrong, in a sentence a person can act on.
    #[serde(rename = "error", alias = "Error", alias = "ERROR", skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,

    #[serde(rename = "message", alias = "Message", alias = "MESSAGE", skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    #[serde(rename = "downloads", alias = "Downloads", alias = "DOWNLOADS", skip_serializing_if = "Option::is_none")]
    pub downloads: Option<Vec<DownloadSummary>>,

    #[serde(rename = "queues", alias = "Queues", alias = "QUEUES", skip_serializing_if = "Option::is_none")]
    pub queues: Option<Vec<QueueSummary>>,

    #[serde(rename = "status", alias = "Status", alias = "STATUS", skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusSummary>,
}

impl CommandResponse {
    pub fn fail(error: impl Into<String>) -> Self {
        Self { ok: false, error: Some(error.into()), ..Default::default() }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadSummary {
    #[serde(rename = "id", alias = "Id", alias = "ID", deserialize_with = "null_as_default")]
    pub id: String,
    #[serde(rename = "fileName", alias = "FileName", alias = "filename", alias = "FILENAME", deserialize_with = "null_as_default")]
    pub file_name: String,
    #[serde(rename = "folder", alias = "Folder", alias = "FOLDER", deserialize_with = "null_as_default")]
    pub folder: String,
    #[serde(rename = "url", alias = "Url", alias = "URL", deserialize_with = "null_as_default")]
    pub url: String,
    #[serde(rename = "status", alias = "Status", alias = "STATUS", deserialize_with = "null_as_default")]
    pub status: String,
    #[serde(rename = "totalBytes", alias = "TotalBytes", alias = "totalbytes", alias = "TOTALBYTES")]
    pub total_bytes: i64,
    #[serde(rename = "downloadedBytes", alias = "DownloadedBytes", alias = "downloadedbytes", alias = "DOWNLOADEDBYTES")]
    pub downloaded_bytes: i64,
    #[serde(rename = "percent", alias = "Percent", alias = "PERCENT", serialize_with = "crate::serde_compat::dotnet_double")]
    pub percent: f64,
    #[serde(
        rename = "bytesPerSecond",
        alias = "BytesPerSecond",
        alias = "bytespersecond",
        alias = "BYTESPERSECOND",
        serialize_with = "crate::serde_compat::dotnet_double"
    )]
    pub bytes_per_second: f64,
    #[serde(rename = "queue", alias = "Queue", alias = "QUEUE", skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    #[serde(rename = "error", alias = "Error", alias = "ERROR", skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueSummary {
    #[serde(rename = "id", alias = "Id", alias = "ID", deserialize_with = "null_as_default")]
    pub id: String,
    #[serde(rename = "name", alias = "Name", alias = "NAME", deserialize_with = "null_as_default")]
    pub name: String,
    #[serde(rename = "state", alias = "State", alias = "STATE", deserialize_with = "null_as_default")]
    pub state: String,
    #[serde(rename = "files", alias = "Files", alias = "FILES")]
    pub files: i32,
    /// Local wall-clock time, no offset (a .NET `DateTime`).
    #[serde(
        rename = "nextRun",
        alias = "NextRun",
        alias = "nextrun",
        alias = "NEXTRUN",
        with = "crate::serde_compat::naive_local",
        skip_serializing_if = "Option::is_none"
    )]
    pub next_run: Option<NaiveDateTime>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusSummary {
    #[serde(rename = "version", alias = "Version", alias = "VERSION", deserialize_with = "null_as_default")]
    pub version: String,
    #[serde(rename = "downloads", alias = "Downloads", alias = "DOWNLOADS")]
    pub downloads: i32,
    #[serde(rename = "active", alias = "Active", alias = "ACTIVE")]
    pub active: i32,
    #[serde(rename = "paused", alias = "Paused", alias = "PAUSED")]
    pub paused: i32,
    #[serde(rename = "queued", alias = "Queued", alias = "QUEUED")]
    pub queued: i32,
    #[serde(rename = "failed", alias = "Failed", alias = "FAILED")]
    pub failed: i32,
    #[serde(rename = "completed", alias = "Completed", alias = "COMPLETED")]
    pub completed: i32,
    #[serde(
        rename = "bytesPerSecond",
        alias = "BytesPerSecond",
        alias = "bytespersecond",
        alias = "BYTESPERSECOND",
        serialize_with = "crate::serde_compat::dotnet_double"
    )]
    pub bytes_per_second: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_written_by_the_c_sharp_qb_reads_whatever_its_case() {
        // PascalCase, nulls included, enum as a number: what the C# pipe sent.
        let json = r#"{"Verb":1,"Urls":["https://a","https://b"],"Folder":"D:\\Ünïcode","FileName":null,"Connections":null,
                       "Queue":null,"Paused":false,"Checksum":null,"Quality":null,"Target":null,"Filter":null}"#;
        let request: CommandRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.verb, CommandVerb::Add);
        assert_eq!(request.urls, ["https://a", "https://b"]);
        assert_eq!(request.folder.as_deref(), Some(r"D:\Ünïcode"));

        let lower: CommandRequest = serde_json::from_str(r#"{"verb":3,"target":"all"}"#).unwrap();
        assert_eq!((lower.verb, lower.target.as_deref()), (CommandVerb::Pause, Some("all")));

        let null_urls: CommandRequest = serde_json::from_str(r#"{"Verb":0,"Urls":null}"#).unwrap();
        assert!(null_urls.urls.is_empty());
    }

    #[test]
    fn a_request_round_trips_in_camel_case_without_unset_parts() {
        let request = CommandRequest { verb: CommandVerb::QueueStart, queue: Some("Night".into()), ..Default::default() };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(json, r#"{"verb":6,"urls":[],"queue":"Night","paused":false}"#);
        assert_eq!(serde_json::from_str::<CommandRequest>(&json).unwrap(), request);
    }

    #[test]
    fn a_response_reads_back_from_either_case() {
        let pascal = r#"{"Ok":true,"Error":null,"Message":"got 2","Downloads":[{"Id":"x","FileName":"a.zip","Folder":"D:\\","Url":"https://a",
                        "Status":"downloading","TotalBytes":10,"DownloadedBytes":5,"Percent":50,"BytesPerSecond":1.5,"Queue":null,"Error":null}],
                        "Queues":[{"Id":"q","Name":"Night","State":"idle","Files":3,"NextRun":"2026-11-09T02:00:00"}],"Status":null}"#;
        let response: CommandResponse = serde_json::from_str(pascal).unwrap();
        assert!(response.ok);
        assert_eq!(response.message.as_deref(), Some("got 2"));
        let download = &response.downloads.as_ref().unwrap()[0];
        assert_eq!((download.percent, download.bytes_per_second), (50.0, 1.5));
        let queue = &response.queues.as_ref().unwrap()[0];
        assert_eq!(queue.next_run.unwrap().to_string(), "2026-11-09 02:00:00");

        let back: CommandResponse = serde_json::from_str(&serde_json::to_string(&response).unwrap()).unwrap();
        assert_eq!(back, response);
    }

    #[test]
    fn whole_doubles_are_written_as_whole_numbers() {
        let summary = DownloadSummary { percent: 100.0, bytes_per_second: 2.5, ..Default::default() };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains(r#""percent":100,"#), "{json}");
        assert!(json.contains(r#""bytesPerSecond":2.5"#), "{json}");
    }

    #[test]
    fn fail_carries_the_error() {
        let response = CommandResponse::fail("nope");
        assert!(!response.ok);
        assert_eq!(response.error.as_deref(), Some("nope"));
    }
}

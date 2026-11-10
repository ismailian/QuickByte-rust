//! Port of `CommandFormatter.cs`: turns a response into what `qb` prints —
//! aligned columns for a person, or the response itself as JSON for a script
//! (`--json`). Pure, and separate from the console so the shape of the output is
//! tested — a script that parses it depends on that shape. The columns use ASCII
//! punctuation: a console on an old code page turns a dash into a question mark.

use super::messages::{CommandRequest, CommandResponse, CommandVerb, DownloadSummary, QueueSummary};
use crate::helpers::byte_formatter::{format_bytes, format_speed};

/// The short id `qb list` shows, and `qb pause` accepts.
pub const SHORT_ID_LENGTH: usize = 8;

/// `Environment.NewLine`, which `AppendLine` wrote.
#[cfg(windows)]
pub const NEWLINE: &str = "\r\n";
#[cfg(not(windows))]
pub const NEWLINE: &str = "\n";

/// The `--json` form: camelCase, indented, without the parts that are not set
/// (the messages' serde attributes carry the naming and the omissions).
pub fn to_json(response: &CommandResponse) -> String {
    serde_json::to_string_pretty(response).unwrap_or_else(|_| "{}".to_string())
}

pub fn format(request: &CommandRequest, response: &CommandResponse, json: bool) -> String {
    if json {
        return to_json(response);
    }

    let mut text = String::new();
    let line = |text: &mut String, s: &str| {
        text.push_str(s);
        text.push_str(NEWLINE);
    };

    if let Some(message) = &response.message {
        line(&mut text, message);
    }

    if let Some(status) = &response.status {
        line(&mut text, format!("QuickByte {}", status.version).trim_end());
        line(
            &mut text,
            &format!(
                "{} downloads: {} active, {} queued, {} paused, {} failed, {} complete",
                status.downloads, status.active, status.queued, status.paused, status.failed, status.completed
            ),
        );
        if status.active > 0 {
            line(&mut text, &format!("Speed: {}", format_speed(status.bytes_per_second)));
        }
    }

    match &response.downloads {
        Some(downloads) if !downloads.is_empty() && matches!(request.verb, CommandVerb::List | CommandVerb::Add) => {
            append_downloads(&mut text, downloads)
        }
        _ if request.verb == CommandVerb::List && response.ok => line(&mut text, "No downloads."),
        _ => {}
    }

    match &response.queues {
        Some(queues) if !queues.is_empty() && request.verb == CommandVerb::Queues => append_queues(&mut text, queues),
        _ if request.verb == CommandVerb::Queues && response.ok => line(&mut text, "No queues."),
        _ => {}
    }

    text.trim_end().to_string()
}

fn append_downloads(text: &mut String, downloads: &[DownloadSummary]) {
    let rows: Vec<Vec<String>> = downloads
        .iter()
        .map(|d| {
            let mut name = d.file_name.clone();
            if let Some(queue) = &d.queue {
                name.push_str(&format!("  [{queue}]"));
            }
            if let Some(error) = &d.error {
                name.push_str(&format!("  - {error}"));
            }
            vec![
                d.id.chars().take(SHORT_ID_LENGTH).collect(),
                d.status.clone(),
                if d.total_bytes > 0 { format!("{:.1}%", d.percent) } else { "-".into() },
                if d.total_bytes > 0 { format_bytes(d.total_bytes) } else { "-".into() },
                if d.bytes_per_second > 0.0 { format_speed(d.bytes_per_second) } else { String::new() },
                name,
            ]
        })
        .collect();
    append_table(text, &["ID", "STATUS", "DONE", "SIZE", "SPEED", "NAME"], &rows);
}

fn append_queues(text: &mut String, queues: &[QueueSummary]) {
    let rows: Vec<Vec<String>> = queues
        .iter()
        .map(|q| {
            vec![
                q.name.clone(),
                q.state.clone(),
                q.files.to_string(),
                q.next_run.map_or_else(|| "-".to_string(), |next| next.format("%a %-d %b %H:%M").to_string()),
            ]
        })
        .collect();
    append_table(text, &["QUEUE", "STATE", "FILES", "NEXT RUN"], &rows);
}

fn append_table(text: &mut String, header: &[&str], rows: &[Vec<String>]) {
    let width = |s: &str| s.chars().count();
    let widths: Vec<usize> = header
        .iter()
        .enumerate()
        .map(|(i, h)| rows.iter().map(|r| width(&r[i])).max().unwrap_or(0).max(width(h)))
        .collect();

    let mut row = |cells: &[&str]| {
        // The last column is not padded: it is the one that may run long.
        let last = cells.len() - 1;
        let line: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, cell)| if i == last { cell.to_string() } else { format!("{cell:<w$}", w = widths[i]) })
            .collect();
        text.push_str(line.join("  ").trim_end());
        text.push_str(NEWLINE);
    };

    row(header);
    for r in rows {
        let cells: Vec<&str> = r.iter().map(String::as_str).collect();
        row(&cells);
    }
}

#[cfg(test)]
mod tests {
    //! What qb prints — a script that reads it depends on its shape.

    use super::*;
    use crate::commands::messages::StatusSummary;

    #[test]
    fn downloads_are_a_table_with_the_short_id_qb_pause_accepts() {
        let response = CommandResponse {
            ok: true,
            downloads: Some(vec![
                DownloadSummary {
                    id: "3f9c0e1a2b4d6f80a1c3e5f7a9b1c3d5".into(),
                    file_name: "ubuntu.iso".into(),
                    status: "downloading".into(),
                    total_bytes: 1024 * 1024,
                    percent: 42.5,
                    bytes_per_second: 2048.0,
                    queue: Some("Night".into()),
                    ..Default::default()
                },
                DownloadSummary {
                    id: "0123456789abcdef0123456789abcdef".into(),
                    file_name: "x.bin".into(),
                    status: "failed".into(),
                    error: Some("404 (Not Found)".into()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        let output = format(&CommandRequest::new(CommandVerb::List), &response, false);
        let lines: Vec<&str> = output.split(NEWLINE).collect();
        assert!(lines[0].starts_with("ID        STATUS       DONE   SIZE     SPEED      NAME"), "{}", lines[0]);
        assert!(lines[1].starts_with("3f9c0e1a  downloading  42.5%  1.00 MB  2.00 KB/s  ubuntu.iso  [Night]"), "{}", lines[1]);
        assert!(lines[2].ends_with("x.bin  - 404 (Not Found)"), "{}", lines[2]);
    }

    #[test]
    fn json_is_the_response_itself_in_camel_case_without_empty_parts() {
        let response = CommandResponse {
            ok: true,
            queues: Some(vec![QueueSummary { id: "q".into(), name: "Night".into(), state: "idle".into(), files: 3, next_run: None }]),
            ..Default::default()
        };
        let json = format(&CommandRequest::new(CommandVerb::Queues), &response, true);
        let document: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(document["ok"], serde_json::Value::Bool(true));
        assert_eq!(document["queues"][0]["name"], "Night");
        assert!(document.get("downloads").is_none());
        assert!(document.get("error").is_none());
        assert!(json.contains("\n  \"ok\": true"), "indented: {json}");
    }

    #[test]
    fn an_empty_list_says_so_rather_than_printing_nothing() {
        let empty_downloads = CommandResponse { ok: true, downloads: Some(Vec::new()), ..Default::default() };
        let empty_queues = CommandResponse { ok: true, queues: Some(Vec::new()), ..Default::default() };
        assert_eq!(format(&CommandRequest::new(CommandVerb::List), &empty_downloads, false), "No downloads.");
        assert_eq!(format(&CommandRequest::new(CommandVerb::Queues), &empty_queues, false), "No queues.");
    }

    #[test]
    fn status_reports_counts_and_speed_only_while_something_runs() {
        let status = |active| CommandResponse {
            ok: true,
            status: Some(StatusSummary {
                version: "1.27.0".into(),
                downloads: 4,
                active,
                queued: 1,
                paused: 1,
                failed: 1,
                completed: 1,
                bytes_per_second: 1536.0,
            }),
            ..Default::default()
        };
        let request = CommandRequest::new(CommandVerb::Status);
        let running = format(&request, &status(1), false);
        assert_eq!(
            running,
            ["QuickByte 1.27.0", "4 downloads: 1 active, 1 queued, 1 paused, 1 failed, 1 complete", "Speed: 1.50 KB/s"].join(NEWLINE)
        );
        assert!(!format(&request, &status(0), false).contains("Speed"));
    }

    #[test]
    fn queues_show_their_next_run_in_a_short_form() {
        let next = chrono::NaiveDate::from_ymd_opt(2026, 11, 9).unwrap().and_hms_opt(2, 0, 0).unwrap();
        let response = CommandResponse {
            ok: true,
            queues: Some(vec![
                QueueSummary { name: "Night".into(), state: "idle".into(), files: 3, next_run: Some(next), ..Default::default() },
                QueueSummary { name: "Work".into(), state: "running".into(), files: 12, next_run: None, ..Default::default() },
            ]),
            ..Default::default()
        };
        let output = format(&CommandRequest::new(CommandVerb::Queues), &response, false);
        let lines: Vec<&str> = output.split(NEWLINE).collect();
        assert_eq!(lines[0], "QUEUE  STATE    FILES  NEXT RUN");
        assert_eq!(lines[1], "Night  idle     3      Mon 9 Nov 02:00");
        assert_eq!(lines[2], "Work   running  12     -");
    }

    #[test]
    fn a_failure_prints_its_message_and_no_empty_list_line() {
        let response = CommandResponse { ok: false, message: Some("Added 1 download.".into()), ..Default::default() };
        assert_eq!(format(&CommandRequest::new(CommandVerb::List), &response, false), "Added 1 download.");
    }
}

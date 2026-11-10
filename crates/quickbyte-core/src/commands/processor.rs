//! Port of `src/QuickByte.Core/Commands/CommandProcessor.cs`.
//!
//! Carries out a [`CommandRequest`] inside the running app, through the same
//! interfaces the windows use — a download added from a prompt is probed, named,
//! reserved, queued and started exactly as one added from Add Download, so there
//! is no second set of rules to drift.
//!
//! Meant to run where everything else that touches the managers runs (the host
//! marshals it there). It never awaits a download: adding one that starts
//! returns as soon as it is under way.

use std::sync::Arc;

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::messages::{CommandRequest, CommandResponse, CommandVerb, DownloadSummary, QueueSummary, StatusSummary};
use crate::enums::{DownloadStatus, QueueState};
use crate::error::{CoreError, Result};
use crate::helpers::file_checksum::{self, ExpectedChecksum};
use crate::helpers::file_name_helper::stem_of;
use crate::helpers::{download_actions, url_credentials};
use crate::models::{DownloadItem, DownloadQueue, DownloadRequest, RemoteFileInfo, RequestOptions, StreamSource, is_path_fully_qualified};
use crate::streaming::stream_detector;
use crate::streaming::{StreamChoice, StreamDescription};
use crate::traits::{DownloadManager, QueueManager, RemoteFileInfoProvider, SettingsService, StreamResolver};

/// How a download is added — the host's own path (the C#
/// `Func<DownloadRequest, Task<DownloadItem>>`), so a download from the command
/// line does not open a details window over whatever the user is doing.
pub type AddFn = Arc<dyn Fn(DownloadRequest) -> BoxFuture<'static, Result<DownloadItem>> + Send + Sync>;

pub struct CommandProcessor {
    downloads: Arc<dyn DownloadManager>,
    queues: Arc<dyn QueueManager>,
    probe: Arc<dyn RemoteFileInfoProvider>,
    streams: Option<Arc<dyn StreamResolver>>,
    settings: Arc<dyn SettingsService>,
    add: AddFn,
    version: String,
}

impl CommandProcessor {
    /// `add`: how a download is added; `None` adds it straight through
    /// `DownloadManager::add_download`.
    pub fn new(
        downloads: Arc<dyn DownloadManager>,
        queues: Arc<dyn QueueManager>,
        probe: Arc<dyn RemoteFileInfoProvider>,
        streams: Option<Arc<dyn StreamResolver>>,
        settings: Arc<dyn SettingsService>,
        add: Option<AddFn>,
        version: impl Into<String>,
    ) -> Self {
        let add = add.unwrap_or_else(|| {
            let downloads = Arc::clone(&downloads);
            Arc::new(move |request: DownloadRequest| {
                let downloads = Arc::clone(&downloads);
                Box::pin(async move { downloads.add_download(request).await }) as BoxFuture<'static, Result<DownloadItem>>
            })
        });
        Self { downloads, queues, probe, streams, settings, add, version: version.into() }
    }

    pub async fn execute(&self, request: CommandRequest, cancel: &CancellationToken) -> CommandResponse {
        let result = match request.verb {
            CommandVerb::Add => self.add(&request, cancel).await,
            CommandVerb::List => Ok(self.list(request.filter.as_deref())),
            CommandVerb::Pause => Ok(self.pause(request.target.as_deref())),
            CommandVerb::Resume => Ok(self.resume(request.target.as_deref())),
            CommandVerb::Queues => Ok(CommandResponse {
                ok: true,
                queues: Some(self.queues.queues().iter().map(|q| self.summarise_queue(q)).collect()),
                ..Default::default()
            }),
            CommandVerb::QueueStart => Ok(self.start_or_stop_queue(request.queue.as_deref(), true)),
            CommandVerb::QueueStop => Ok(self.start_or_stop_queue(request.queue.as_deref(), false)),
            CommandVerb::Status => Ok(self.status()),
        };
        result.unwrap_or_else(|e| CommandResponse::fail(e.to_string()))
    }

    // ------------------------------------------------------------------- add --

    async fn add(&self, request: &CommandRequest, cancel: &CancellationToken) -> Result<CommandResponse> {
        if let Some(folder) = &request.folder
            && !is_path_fully_qualified(folder)
        {
            return Ok(CommandResponse::fail(format!("'{folder}' is not a full folder path.")));
        }

        let mut checksum = None;
        if let Some(text) = &request.checksum {
            checksum = file_checksum::parse(Some(text));
            if checksum.is_none() {
                return Ok(CommandResponse::fail("That checksum is not one QuickByte recognises. Give the hex value, or sha256:<hex>."));
            }
        }

        let mut queue_id = None;
        if let Some(name) = request.queue.as_deref().filter(|q| !q.trim().is_empty()) {
            queue_id = Some(match self.find_queue(Some(name)) {
                Ok(Some(queue)) => queue.id,
                _ => self.queues.create(name.trim()).id,
            });
        }

        let mut added = Vec::new();
        let mut failures = Vec::new();
        for link in &request.urls {
            match self.add_one(link, request, checksum.as_ref(), cancel).await {
                Ok(item) => {
                    if let Some(id) = queue_id {
                        self.queues.add_to_queue(id, &[item.id]);
                    } else if !request.paused {
                        let downloads = Arc::clone(&self.downloads);
                        let id = item.id;
                        tokio::spawn(async move {
                            let _ = downloads.start(id).await;
                        });
                    }
                    added.push(self.summarise(&item));
                }
                Err(e) => failures.push(format!("{link}: {}", explain(&e))),
            }
        }

        let place = match queue_id {
            None if request.paused => "paused".to_string(),
            None => "started".to_string(),
            Some(id) => format!("in queue \"{}\"", self.queues.find(id).map(|q| q.name).unwrap_or_default()),
        };
        Ok(CommandResponse {
            ok: failures.is_empty(),
            message: (!added.is_empty()).then(|| format!("Added {} download{}, {place}.", added.len(), plural(added.len()))),
            downloads: Some(added),
            error: (!failures.is_empty()).then(|| failures.join(crate::commands::formatter::NEWLINE)),
            ..Default::default()
        })
    }

    async fn add_one(&self, link: &str, request: &CommandRequest, checksum: Option<&ExpectedChecksum>, cancel: &CancellationToken) -> Result<DownloadItem> {
        // Lifted out of the address, as Add Download does: the address is
        // persisted and displayed, and only the credential field encrypts itself.
        let split = url_credentials::extract(link);
        let scheme_ok = url::Url::parse(&split.url).is_ok_and(|u| matches!(u.scheme(), "http" | "https" | "ftp" | "ftps"));
        if !scheme_ok {
            return Err(CoreError::invalid("not an http, https, ftp or ftps link"));
        }

        let options = RequestOptions { credentials: split.credentials.clone(), ..Default::default() };
        let mut info = self.probe.get_file_info(&split.url, &options, cancel).await?;

        let mut stream: Option<StreamSource> = None;
        let mut file_name = request.file_name.clone().unwrap_or_else(|| info.file_name.clone());
        let mut splittable = info.supports_range_requests;

        if let Some(streams) = &self.streams
            && (stream_detector::is_stream(&info.final_url, Some(&info.content_type)) || stream_detector::is_stream(&split.url, None))
        {
            let description = streams.describe(&split.url, &options, cancel).await?;
            let Some(choice) = choose_quality(&description, request.quality.as_deref()) else {
                let offered: Vec<&str> = description.choices.iter().map(|c| c.label.as_str()).collect();
                return Err(CoreError::invalid(format!(
                    "the video offers no quality matching \"{}\" ({})",
                    request.quality.as_deref().unwrap_or(""),
                    offered.join(", ")
                )));
            };
            stream = Some(choice.source.clone());
            file_name = match &request.file_name {
                Some(named) => format!("{}{}", stem_of(named), choice.extension),
                None => stream_detector::suggest_file_name(&split.url, &info.file_name, &choice.extension),
            };
            info = RemoteFileInfo {
                file_name: file_name.clone(),
                content_type: choice.content_type.clone(),
                final_url: info.final_url.clone(),
                supports_range_requests: true,
                ..Default::default()
            };
            splittable = true;
        }

        let settings = self.settings.current();
        let connections = if splittable { settings.clamp_connections(request.connections.unwrap_or(settings.default_connections_count)) } else { 1 };
        let folder = request.folder.clone().unwrap_or_else(|| settings.folder_for(&file_name));

        let mut add = DownloadRequest::new(split.url.clone(), info, folder, file_name, connections);
        add.credentials = split.credentials;
        add.expected_checksum = checksum.cloned();
        add.start_immediately = false;
        add.stream = stream;
        (self.add)(add).await
    }

    // ----------------------------------------------------------------- state --

    fn list(&self, filter: Option<&str>) -> CommandResponse {
        let items: Vec<DownloadSummary> = self
            .downloads
            .downloads()
            .iter()
            .filter(|item| match filter.unwrap_or("all") {
                "active" => download_actions::is_active(item),
                "paused" => item.status == DownloadStatus::Paused,
                "queued" => item.status == DownloadStatus::Queued,
                "completed" => item.status == DownloadStatus::Completed,
                "failed" => matches!(item.status, DownloadStatus::Failed | DownloadStatus::Cancelled),
                _ => true,
            })
            .map(|i| self.summarise(i))
            .collect();
        CommandResponse { ok: true, downloads: Some(items), ..Default::default() }
    }

    fn status(&self) -> CommandResponse {
        let items = self.downloads.downloads();
        let count = |f: &dyn Fn(&DownloadItem) -> bool| items.iter().filter(|i| f(i)).count() as i32;
        CommandResponse {
            ok: true,
            status: Some(StatusSummary {
                version: self.version.clone(),
                downloads: items.len() as i32,
                active: count(&download_actions::is_active),
                paused: count(&|i| i.status == DownloadStatus::Paused),
                queued: count(&|i| i.status == DownloadStatus::Queued),
                failed: count(&|i| matches!(i.status, DownloadStatus::Failed | DownloadStatus::Cancelled)),
                completed: count(&|i| i.status == DownloadStatus::Completed),
                bytes_per_second: items.iter().filter(|i| i.status == DownloadStatus::Downloading).map(|i| i.current_speed_bytes_per_second).sum(),
            }),
            ..Default::default()
        }
    }

    fn pause(&self, target: Option<&str>) -> CommandResponse {
        if is_all(target) {
            let paused = self.downloads.pause_all();
            return CommandResponse { ok: true, message: Some(format!("Paused {paused} download{}.", plural(paused))), ..Default::default() };
        }
        let item = match self.find_download(target) {
            Ok(item) => item,
            Err(problem) => return CommandResponse::fail(problem),
        };
        if !download_actions::can_pause(&item) {
            return CommandResponse::fail(format!("{} is {}, so there is nothing to pause.", item.file_name, describe(item.status)));
        }
        self.downloads.pause(item.id);
        CommandResponse { ok: true, message: Some(format!("Paused {}.", item.file_name)), downloads: Some(vec![self.summarise(&item)]), ..Default::default() }
    }

    fn resume(&self, target: Option<&str>) -> CommandResponse {
        if is_all(target) {
            let resumable: Vec<Uuid> = self.downloads.downloads().iter().filter(|i| download_actions::can_resume(i)).map(|i| i.id).collect();
            for &id in &resumable {
                self.spawn_resume(id);
            }
            return CommandResponse {
                ok: true,
                message: Some(format!("Resumed {} download{}.", resumable.len(), plural(resumable.len()))),
                ..Default::default()
            };
        }
        let found = match self.find_download(target) {
            Ok(item) => item,
            Err(problem) => return CommandResponse::fail(problem),
        };
        if !download_actions::can_resume(&found) {
            return CommandResponse::fail(format!("{} is {}, so there is nothing to resume.", found.file_name, describe(found.status)));
        }
        self.spawn_resume(found.id);
        CommandResponse { ok: true, message: Some(format!("Resumed {}.", found.file_name)), downloads: Some(vec![self.summarise(&found)]), ..Default::default() }
    }

    fn spawn_resume(&self, id: Uuid) {
        let downloads = Arc::clone(&self.downloads);
        tokio::spawn(async move {
            let _ = downloads.resume(id).await;
        });
    }

    fn start_or_stop_queue(&self, name: Option<&str>, start: bool) -> CommandResponse {
        let queue = match self.find_queue(name) {
            Ok(Some(queue)) => queue,
            Ok(None) => return CommandResponse::fail(format!("There is no queue called \"{}\".", name.unwrap_or(""))),
            Err(problem) => return CommandResponse::fail(problem),
        };
        if start {
            self.queues.start(queue.id);
        } else {
            self.queues.stop(queue.id);
        }
        CommandResponse {
            ok: true,
            message: Some(format!("{} queue \"{}\".", if start { "Started" } else { "Stopped" }, queue.name)),
            queues: Some(vec![self.summarise_queue(&queue)]),
            ..Default::default()
        }
    }

    // --------------------------------------------------------------- finding --

    /// By id, or the start of one — as `qb list` prints it — as long as only one
    /// download starts that way.
    fn find_download(&self, target: Option<&str>) -> std::result::Result<DownloadItem, String> {
        let key = target.unwrap_or("").trim().replace('-', "").to_lowercase();
        if key.chars().count() < 4 {
            return Err("Give at least the first four characters of the download's id, as 'qb list' shows it.".into());
        }
        let matches: Vec<DownloadItem> =
            self.downloads.downloads().into_iter().filter(|i| i.id.simple().to_string().starts_with(&key)).take(2).collect();
        match matches.len() {
            1 => Ok(matches.into_iter().next().expect("one")),
            0 => Err(format!("No download has an id starting '{}'.", target.unwrap_or(""))),
            _ => Err(format!("More than one download has an id starting '{}'; give more of it.", target.unwrap_or(""))),
        }
    }

    /// By name, ignoring case; or the one queue whose name starts that way.
    /// `Err` names the ambiguity; `Ok(None)` is no such queue.
    fn find_queue(&self, name: Option<&str>) -> std::result::Result<Option<DownloadQueue>, String> {
        let wanted = name.unwrap_or("").trim().to_lowercase();
        let queues = self.queues.queues();
        if let Some(exact) = queues.iter().find(|q| q.name.to_lowercase() == wanted) {
            return Ok(Some(exact.clone()));
        }
        if wanted.is_empty() {
            return Ok(None);
        }
        let prefixed: Vec<&DownloadQueue> = queues.iter().filter(|q| q.name.to_lowercase().starts_with(&wanted)).take(2).collect();
        match prefixed.len() {
            1 => Ok(Some(prefixed[0].clone())),
            0 => Ok(None),
            _ => Err(format!("More than one queue's name starts with \"{}\".", name.unwrap_or("").trim())),
        }
    }

    // ------------------------------------------------------------- summaries --

    fn summarise(&self, item: &DownloadItem) -> DownloadSummary {
        DownloadSummary {
            id: item.id.simple().to_string(),
            file_name: item.file_name.clone(),
            folder: item.save_folder.clone(),
            url: item.url.clone(),
            status: describe(item.status),
            total_bytes: item.total_bytes,
            downloaded_bytes: item.downloaded_bytes,
            percent: round_half_even_1(item.progress_percentage()),
            bytes_per_second: if item.status == DownloadStatus::Downloading { item.current_speed_bytes_per_second } else { 0.0 },
            queue: self.queues.queue_of(item.id).map(|q| q.name),
            error: if item.status == DownloadStatus::Failed { item.error_message.clone() } else { None },
        }
    }

    fn summarise_queue(&self, queue: &DownloadQueue) -> QueueSummary {
        QueueSummary {
            id: queue.id.simple().to_string(),
            name: queue.name.clone(),
            state: match self.queues.state_of(queue.id) {
                QueueState::Idle => "idle",
                QueueState::Running => "running",
            }
            .into(),
            files: queue.item_ids.len() as i32,
            next_run: self.queues.next_run_at(queue.id),
        }
    }
}

/// "best" (the default) is the first offered, "worst" the last video, anything
/// else the first label containing it.
pub fn choose_quality<'a>(description: &'a StreamDescription, quality: Option<&str>) -> Option<&'a StreamChoice> {
    let choices = &description.choices;
    let first = choices.first()?;
    let quality = quality.map(str::trim).unwrap_or("");
    if quality.is_empty() || quality.eq_ignore_ascii_case("best") {
        return Some(first);
    }
    if quality.eq_ignore_ascii_case("worst") {
        return choices.iter().rev().find(|c| !c.label.starts_with("Audio only")).or(choices.last());
    }
    let wanted = quality.to_lowercase();
    choices.iter().find(|c| c.label.to_lowercase().contains(&wanted))
}

fn explain(error: &CoreError) -> String {
    match error {
        CoreError::AuthenticationRequired { .. } => {
            "it needs a user name and password; put them in the link (https://user:password@host/...)".into()
        }
        other => other.to_string(),
    }
}

fn is_all(target: Option<&str>) -> bool {
    target.is_some_and(|t| t.eq_ignore_ascii_case("all"))
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn describe(status: DownloadStatus) -> String {
    status.name().to_lowercase()
}

/// `Math.Round(x, 1)` — banker's rounding, as .NET does by default.
fn round_half_even_1(value: f64) -> f64 {
    let scaled = value * 10.0;
    let rounded = scaled.round();
    let result = if (scaled - scaled.trunc()).abs() == 0.5 { 2.0 * (scaled / 2.0).round() } else { rounded };
    result / 10.0
}

#[cfg(test)]
mod tests;

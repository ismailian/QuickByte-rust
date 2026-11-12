//! The background VirusTotal check, run for every download added while it is
//! switched on (Options > VirusTotal). Nothing waits on it: the download starts
//! at once, and the check runs beside it.
//!
//! 1. **The address**, as soon as the download is added: VirusTotal's report
//!    on it if it has one, else a fresh analysis.
//! 2. If that comes back malicious or suspicious **while the download is still
//!    running**, the verdict's action applies — by default the download is
//!    paused and the user asked whether to delete it or carry on.
//! 3. Otherwise, **once the download is complete**, the file's SHA-256 is
//!    looked up.
//! 4. If VirusTotal has never seen the file, it is **uploaded** — when it is no
//!    larger than the upload limit (25 MB at most) — and the analysis awaited.
//!    A malicious or suspicious verdict on the file gets the same actions; the
//!    choice is then keep-or-delete.
//!
//! A download that came from a private source (a login, the browser's cookies)
//! skips steps 1 and 4 unless the user opted in to sharing those too; see
//! `helpers::virus_total` for exactly what is sent.
//!
//! Each step is recorded on the download (`DownloadItem::virus_total`) as it
//! finishes, so a check the app closed in the middle of carries on at the next
//! launch from where it got to — including a question it was still waiting on
//! an answer to. Turning the check off stops every check at its next step; one
//! turned back on picks up where it left off.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::enums::{DownloadStatus, VirusTotalAction};
use crate::error::{CoreError, Result};
use crate::events::{
    DownloadEvent, DownloadListChangeType, Dispatcher, EventHub, EventStream, VirusTotalEvent, VirusTotalNotice,
};
use crate::helpers::byte_formatter::format_bytes;
use crate::helpers::download_actions;
use crate::helpers::file_checksum::{self, ChecksumAlgorithm};
use crate::helpers::virus_total::{self, ScanResult, classify, file_link, url_link};
use crate::models::{DownloadItem, DownloadSettings, VirusTotalReport, VirusTotalSource, VirusTotalStep, VirusTotalVerdict};
use crate::net::virus_total::KEY_REJECTED;
use crate::serde_compat::DateTimeOffset;
use crate::traits::{self, DownloadManager, SettingsService, VirusTotalApi};

/// How patiently an analysis is waited for.
#[derive(Clone, Debug)]
pub struct ScannerOptions {
    /// Between asks whether an analysis has finished (on top of the client's own pacing).
    pub poll_interval: Duration,
    /// How many times an address's analysis is asked after (about five minutes).
    pub url_polls: u32,
    /// How many times an upload's analysis is asked after (about ten minutes).
    pub file_polls: u32,
}

impl Default for ScannerOptions {
    fn default() -> Self {
        Self { poll_interval: Duration::from_secs(20), url_polls: 15, file_polls: 30 }
    }
}

/// One download being checked.
struct Scan {
    /// Wakes the check when the download's status changes (it waits for Completed).
    status: watch::Sender<DownloadStatus>,
    /// Cancelled when the download is removed.
    cancel: CancellationToken,
    /// Set while the user is being asked; their answer goes through it.
    answer: Option<oneshot::Sender<bool>>,
}

pub struct VirusTotalScanner {
    me: Weak<VirusTotalScanner>,
    downloads: Arc<dyn DownloadManager>,
    settings: Arc<dyn SettingsService>,
    api: Arc<dyn VirusTotalApi>,
    runtime: Handle,
    options: ScannerOptions,
    scans: Mutex<HashMap<Uuid, Scan>>,
    started: AtomicBool,
    hub: EventHub<VirusTotalEvent>,
}

impl VirusTotalScanner {
    pub fn new(
        downloads: Arc<dyn DownloadManager>,
        settings: Arc<dyn SettingsService>,
        api: Arc<dyn VirusTotalApi>,
        dispatcher: Option<Arc<dyn Dispatcher>>,
        runtime: Handle,
        options: ScannerOptions,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            downloads,
            settings,
            api,
            runtime,
            options,
            scans: Mutex::new(HashMap::new()),
            started: AtomicBool::new(false),
            hub: EventHub::new(dispatcher),
        })
    }

    /// The settings, if checks may run right now.
    fn ready(&self) -> Option<Arc<DownloadSettings>> {
        Some(self.settings.current()).filter(|s| s.virus_total_ready())
    }

    /// Whether a check is running (or waiting for an answer) for a download.
    pub fn is_checking(&self, download_id: Uuid) -> bool {
        self.scans.lock().contains_key(&download_id)
    }

    // Inline, on the publishing thread: quick, and nothing here calls back into
    // the download manager.
    fn on_download_event(&self, event: &DownloadEvent) {
        match event {
            DownloadEvent::ListChanged { change: DownloadListChangeType::Added, item } => {
                if item.virus_total.is_none() && self.settings.current().virus_total_ready() {
                    self.begin(item.id, item.status);
                }
            }
            DownloadEvent::ListChanged { change: DownloadListChangeType::Removed, item } => {
                if let Some(scan) = self.scans.lock().remove(&item.id) {
                    scan.cancel.cancel();
                }
            }
            DownloadEvent::StatusChanged(change) => {
                if let Some(scan) = self.scans.lock().get(&change.download_id) {
                    scan.status.send_replace(change.new_status);
                }
            }
            _ => {}
        }
    }

    /// Carries on every check that has not finished — at startup, and when the
    /// check is switched (back) on.
    fn resume_unfinished(&self) {
        for item in self.downloads.downloads() {
            if item.virus_total.as_ref().is_some_and(VirusTotalReport::is_unfinished) {
                self.begin(item.id, item.status);
            }
        }
    }

    fn begin(&self, id: Uuid, status: DownloadStatus) {
        let (status_tx, status_rx) = watch::channel(status);
        let cancel = CancellationToken::new();
        {
            let mut scans = self.scans.lock();
            if scans.contains_key(&id) {
                return;
            }
            scans.insert(id, Scan { status: status_tx, cancel: cancel.clone(), answer: None });
        }
        let Some(me) = self.me.upgrade() else { return };
        self.runtime.spawn(async move {
            me.run(id, status_rx, &cancel).await;
            me.scans.lock().remove(&id);
        });
    }

    async fn run(&self, id: Uuid, mut status: watch::Receiver<DownloadStatus>, cancel: &CancellationToken) {
        let Some(item) = self.downloads.get(id) else { return };
        let mut report = match item.virus_total.clone() {
            Some(report) => report,
            None => {
                let report = VirusTotalReport::started();
                self.save(id, &report);
                report
            }
        };

        // A question the app closed before it was answered is asked again.
        if report.awaits_decision() {
            self.act(id, &mut report, cancel).await;
            return;
        }

        if report.step == VirusTotalStep::Url {
            let Some(settings) = self.ready() else { return };
            self.check_address(&item, &settings, &mut report, cancel).await;
            if cancel.is_cancelled() {
                return;
            }
            if report.verdict.is_negative() {
                self.save(id, &report);
                self.act(id, &mut report, cancel).await;
                return;
            }
            if report.step != VirusTotalStep::Done {
                report.step = VirusTotalStep::File;
            }
            self.save(id, &report);
        }
        if report.step != VirusTotalStep::File {
            return;
        }

        if !self.until_complete(id, &mut status, cancel).await {
            return;
        }
        let Some(settings) = self.ready() else { return };
        let Some(item) = self.downloads.get(id) else { return };
        self.check_file(&item, &settings, &mut report, cancel).await;
        if cancel.is_cancelled() {
            return;
        }
        if report.verdict == VirusTotalVerdict::Pending {
            report.verdict = VirusTotalVerdict::Unknown;
        }
        if report.verdict.is_negative() {
            self.save(id, &report);
            self.act(id, &mut report, cancel).await;
            return;
        }
        report.step = VirusTotalStep::Done;
        report.checked_at.get_or_insert_with(DateTimeOffset::now);
        self.save(id, &report);
    }

    // ------------------------------------------------------------ the steps --

    /// Step 1: the download's address.
    async fn check_address(&self, item: &DownloadItem, settings: &DownloadSettings, report: &mut VirusTotalReport, cancel: &CancellationToken) {
        if !virus_total::may_share(item, settings) {
            return;
        }
        let Some(address) = virus_total::shareable_url(&item.url) else { return };
        match self.address_result(&settings.virus_total_api_key, &address, cancel).await {
            Ok(Some(result)) => record(report, &result, settings.virus_total_threshold(), VirusTotalSource::Url, None),
            Ok(None) => {}
            Err(e) if e.is_cancelled() => {}
            Err(e) => note_error(report, &e, "The download's address could not be checked"),
        }
    }

    async fn address_result(&self, api_key: &str, address: &str, cancel: &CancellationToken) -> Result<Option<ScanResult>> {
        if let Some(known) = self.api.url_report(api_key, address, cancel).await?
            && known.stats.engines() > 0
        {
            return Ok(Some(known));
        }
        let analysis = self.api.submit_url(api_key, address, cancel).await?;
        self.wait_for(api_key, &analysis, self.options.url_polls, cancel).await
    }

    /// Steps 3 and 4: the finished file, by hash and then by upload.
    async fn check_file(&self, item: &DownloadItem, settings: &DownloadSettings, report: &mut VirusTotalReport, cancel: &CancellationToken) {
        let path = item.full_path();
        let Ok(size) = std::fs::metadata(&path).map(|m| m.len()) else {
            report.note = Some("The finished file was not there to check.".into());
            return;
        };
        let sha256 = match file_checksum::compute(&path, ChecksumAlgorithm::Sha256, None, cancel).await {
            Ok(hash) => hash,
            Err(e) => {
                if !e.is_cancelled() {
                    report.note = Some(format!("The file could not be read to check it: {e}"));
                }
                return;
            }
        };
        report.sha256 = Some(sha256.clone());
        let api_key = &settings.virus_total_api_key;
        let result = match self.api.file_report(api_key, &sha256, cancel).await {
            Ok(Some(known)) if known.stats.engines() > 0 => Some(known),
            Ok(_) => self.upload(item, settings, &path, size, report, cancel).await,
            Err(e) => {
                note_error(report, &e, "The file could not be looked up");
                None
            }
        };
        if let Some(result) = result {
            record(report, &result, settings.virus_total_threshold(), VirusTotalSource::File, Some(&sha256));
        }
    }

    async fn upload(
        &self,
        item: &DownloadItem,
        settings: &DownloadSettings,
        path: &str,
        size: u64,
        report: &mut VirusTotalReport,
        cancel: &CancellationToken,
    ) -> Option<ScanResult> {
        let limit = settings.virus_total_upload_limit_bytes();
        if size as i64 > limit {
            report.note = Some(format!(
                "VirusTotal has not seen this file, and at {} it is over the {} upload limit, so it was not uploaded.",
                format_bytes(size as i64),
                format_bytes(limit)
            ));
            return None;
        }
        if !virus_total::may_share(item, settings) {
            report.note = Some(
                "VirusTotal has not seen this file. It came from a private source, so it was not uploaded \
                 (Options > VirusTotal can allow that)."
                    .into(),
            );
            return None;
        }
        let api_key = &settings.virus_total_api_key;
        let outcome = match self.api.upload_file(api_key, path, cancel).await {
            Ok(analysis) => self.wait_for(api_key, &analysis, self.options.file_polls, cancel).await,
            Err(e) => Err(e),
        };
        match outcome {
            Ok(Some(result)) => Some(result),
            Ok(None) => {
                report.note = Some("The file was uploaded, but VirusTotal had not finished analysing it. Its report will be on VirusTotal.".into());
                report.permalink = report.sha256.as_deref().map(file_link);
                None
            }
            Err(e) if e.is_cancelled() => None,
            Err(e) => {
                note_error(report, &e, "The file could not be uploaded");
                None
            }
        }
    }

    /// Asks after an analysis until it finishes or `polls` run out (`Ok(None)`).
    async fn wait_for(&self, api_key: &str, analysis: &str, polls: u32, cancel: &CancellationToken) -> Result<Option<ScanResult>> {
        for _ in 0..polls {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                _ = tokio::time::sleep(self.options.poll_interval) => {}
            }
            if let Some(result) = self.api.analysis(api_key, analysis, cancel).await? {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    /// Waits for the download to complete. False if it was removed first. A
    /// download that fails or is stopped is waited on still: it may be retried.
    async fn until_complete(&self, id: Uuid, status: &mut watch::Receiver<DownloadStatus>, cancel: &CancellationToken) -> bool {
        loop {
            // The download itself is the truth; the channel only says when to look.
            match self.downloads.get(id) {
                None => return false,
                Some(item) if item.status == DownloadStatus::Completed => return true,
                Some(_) => {}
            }
            tokio::select! {
                _ = cancel.cancelled() => return false,
                changed = status.changed() => if changed.is_err() { return false },
            }
        }
    }

    // --------------------------------------------------------- the verdict --

    /// Applies the action Options chose for a malicious or suspicious verdict.
    async fn act(&self, id: Uuid, report: &mut VirusTotalReport, cancel: &CancellationToken) {
        let Some(item) = self.downloads.get(id) else { return };
        let notice = VirusTotalNotice {
            download_id: id,
            file_name: item.file_name.clone(),
            report: report.clone(),
            finished: item.status == DownloadStatus::Completed,
        };
        match self.settings.current().virus_total_action(report.verdict) {
            None => {
                report.step = VirusTotalStep::Done;
                self.save(id, report);
            }
            Some(VirusTotalAction::Warn) => {
                report.step = VirusTotalStep::Done;
                self.save(id, report);
                self.hub.publish(VirusTotalEvent::Warned(notice));
            }
            Some(VirusTotalAction::Delete) => self.delete(id, notice, true),
            Some(VirusTotalAction::Ask) => {
                if download_actions::can_pause(&item) {
                    self.downloads.pause(id);
                }
                let (answer_tx, answer_rx) = oneshot::channel();
                match self.scans.lock().get_mut(&id) {
                    Some(scan) => scan.answer = Some(answer_tx),
                    None => return,
                }
                self.hub.publish(VirusTotalEvent::DecisionNeeded(notice.clone()));
                let keep = tokio::select! {
                    _ = cancel.cancelled() => return,
                    answer = answer_rx => match answer {
                        Ok(keep) => keep,
                        Err(_) => return,
                    },
                };
                if keep {
                    self.keep(id, report);
                } else {
                    self.delete(id, notice, false);
                }
            }
        }
    }

    /// The user kept a flagged download: recorded, and carried on with if the
    /// check paused it.
    fn keep(&self, id: Uuid, report: &mut VirusTotalReport) {
        report.kept_by_user = true;
        report.step = VirusTotalStep::Done;
        self.save(id, report);
        if self.downloads.get(id).is_some_and(|i| i.status == DownloadStatus::Paused) {
            let downloads = Arc::clone(&self.downloads);
            // Spawned: resuming runs the whole rest of the download.
            self.runtime.spawn(async move {
                let _ = downloads.resume(id).await;
            });
        }
    }

    fn delete(&self, id: Uuid, notice: VirusTotalNotice, automatically: bool) {
        self.downloads.remove(id, true);
        self.hub.publish(VirusTotalEvent::Deleted { notice, automatically });
    }

    fn save(&self, id: Uuid, report: &VirusTotalReport) {
        self.downloads.set_virus_total(id, report.clone());
    }
}

/// Puts a finished lookup on the report. A tally nobody voted in is no verdict
/// and changes nothing.
fn record(report: &mut VirusTotalReport, result: &ScanResult, threshold: i32, source: VirusTotalSource, sha256: Option<&str>) {
    let verdict = classify(&result.stats, threshold);
    if verdict == VirusTotalVerdict::Unknown {
        return;
    }
    report.verdict = verdict;
    report.source = Some(source);
    report.malicious = result.stats.malicious;
    report.suspicious = result.stats.suspicious;
    report.engines = result.stats.engines();
    report.permalink = match source {
        VirusTotalSource::File => sha256.map(file_link),
        VirusTotalSource::Url => result.object_id.as_deref().map(url_link),
    };
    report.note = None;
    report.checked_at = Some(DateTimeOffset::now());
}

/// Records why a step got no answer. A key VirusTotal refuses fails every
/// later step the same way, so it ends the check.
fn note_error(report: &mut VirusTotalReport, error: &CoreError, context: &str) {
    if matches!(error, CoreError::AuthenticationRequired { .. }) {
        report.note = Some(KEY_REJECTED.into());
        report.step = VirusTotalStep::Done;
        if report.verdict == VirusTotalVerdict::Pending {
            report.verdict = VirusTotalVerdict::Unknown;
        }
    } else {
        report.note = Some(format!("{context}: {error}"));
    }
}

#[async_trait]
impl traits::VirusTotalService for VirusTotalScanner {
    fn subscribe(&self) -> EventStream<VirusTotalEvent> {
        self.hub.subscribe()
    }

    fn start(&self) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.me.clone();
        self.downloads.subscribe_fn(Arc::new(move |event: &DownloadEvent| {
            if let Some(scanner) = me.upgrade() {
                scanner.on_download_event(event);
            }
        }));
        let me = self.me.clone();
        self.settings.subscribe_fn(Arc::new(move |settings: &Arc<DownloadSettings>| {
            if settings.virus_total_ready()
                && let Some(scanner) = me.upgrade()
            {
                scanner.resume_unfinished();
            }
        }));
        if self.ready().is_some() {
            self.resume_unfinished();
        }
    }

    fn resolve(&self, download_id: Uuid, keep: bool) {
        let waiting = self.scans.lock().get_mut(&download_id).and_then(|s| s.answer.take());
        if let Some(answer) = waiting {
            let _ = answer.send(keep);
            return;
        }
        // Nothing is waiting — the check is switched off now, say. The answer
        // still stands.
        let Some(item) = self.downloads.get(download_id) else { return };
        let mut report = item.virus_total.clone().unwrap_or_default();
        if keep {
            self.keep(download_id, &mut report);
        } else {
            self.delete(
                download_id,
                VirusTotalNotice {
                    download_id,
                    file_name: item.file_name.clone(),
                    report,
                    finished: item.status == DownloadStatus::Completed,
                },
                false,
            );
        }
    }

    async fn check_api_key(&self, api_key: &str, cancel: &CancellationToken) -> Result<()> {
        self.api.check_key(api_key, cancel).await
    }
}

#[cfg(test)]
mod tests;

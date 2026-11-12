//! The check's workflow, against a fake VirusTotal and a fake download list:
//! what is sent when, what pauses, what is asked, and what is never sent.

use std::sync::atomic::AtomicBool;

use async_trait::async_trait;

use super::*;
use crate::enums::{CompletionActions, ConnectionCommand, PowerAction};
use crate::events::{DownloadStatusChanged, EventCallback};
use crate::helpers::disk_space::scratch::TempFolder;
use crate::helpers::file_checksum::{ChecksumResult, ExpectedChecksum};
use crate::helpers::virus_total::ScanStats;
use crate::models::{AddressChange, ConnectionInfo, DownloadCredentials, DownloadRequest};
use crate::traits::{ProgressFn, VirusTotalService as _};

const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn result(malicious: i32, suspicious: i32, harmless: i32) -> Option<ScanResult> {
    Some(ScanResult {
        stats: ScanStats { malicious, suspicious, harmless, undetected: 5, timeout: 0 },
        object_id: Some("object".into()),
    })
}

fn clean() -> Option<ScanResult> {
    result(0, 0, 60)
}

// ------------------------------------------------------------------ fakes --

#[derive(Default)]
struct FakeApi {
    url_report: Mutex<Option<ScanResult>>,
    url_analysis: Mutex<Option<ScanResult>>,
    file_report: Mutex<Option<ScanResult>>,
    file_analysis: Mutex<Option<ScanResult>>,
    reject_key: AtomicBool,
    calls: Mutex<Vec<String>>,
}

impl FakeApi {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().clone()
    }
    fn called(&self, prefix: &str) -> bool {
        self.calls().iter().any(|c| c.starts_with(prefix))
    }
    fn log(&self, call: String) -> Result<()> {
        self.calls.lock().push(call);
        if self.reject_key.load(Ordering::SeqCst) { Err(CoreError::auth(KEY_REJECTED, true)) } else { Ok(()) }
    }
}

#[async_trait]
impl VirusTotalApi for FakeApi {
    async fn url_report(&self, _key: &str, url: &str, _c: &CancellationToken) -> Result<Option<ScanResult>> {
        self.log(format!("url_report {url}"))?;
        Ok(self.url_report.lock().clone())
    }
    async fn submit_url(&self, _key: &str, url: &str, _c: &CancellationToken) -> Result<String> {
        self.log(format!("submit_url {url}"))?;
        Ok("url-analysis".into())
    }
    async fn analysis(&self, _key: &str, id: &str, _c: &CancellationToken) -> Result<Option<ScanResult>> {
        self.log(format!("analysis {id}"))?;
        Ok(if id == "url-analysis" { self.url_analysis.lock().clone() } else { self.file_analysis.lock().clone() })
    }
    async fn file_report(&self, _key: &str, sha256: &str, _c: &CancellationToken) -> Result<Option<ScanResult>> {
        self.log(format!("file_report {sha256}"))?;
        Ok(self.file_report.lock().clone())
    }
    async fn upload_file(&self, _key: &str, path: &str, _c: &CancellationToken) -> Result<String> {
        self.log(format!("upload {path}"))?;
        Ok("file-analysis".into())
    }
    async fn check_key(&self, _key: &str, _c: &CancellationToken) -> Result<()> {
        self.log("check_key".into())
    }
}

struct FakeSettings {
    current: Mutex<Arc<DownloadSettings>>,
    hub: EventHub<Arc<DownloadSettings>>,
}

impl SettingsService for FakeSettings {
    fn current(&self) -> Arc<DownloadSettings> {
        self.current.lock().clone()
    }
    fn subscribe(&self) -> EventStream<Arc<DownloadSettings>> {
        self.hub.subscribe()
    }
    fn subscribe_fn(&self, callback: EventCallback<Arc<DownloadSettings>>) {
        self.hub.subscribe_fn(callback);
    }
    fn load(&self) {}
    fn save(&self, settings: DownloadSettings) {
        let settings = Arc::new(settings);
        *self.current.lock() = settings.clone();
        self.hub.publish(settings);
    }
}

#[derive(Default)]
struct FakeDownloads {
    items: Mutex<Vec<DownloadItem>>,
    paused: Mutex<Vec<Uuid>>,
    resumed: Mutex<Vec<Uuid>>,
    removed: Mutex<Vec<(Uuid, bool)>>,
    hub: EventHub<DownloadEvent>,
}

impl FakeDownloads {
    fn add(&self, item: DownloadItem) -> Uuid {
        let id = item.id;
        self.items.lock().push(item.clone());
        self.hub.publish(DownloadEvent::ListChanged { change: DownloadListChangeType::Added, item: Box::new(item) });
        id
    }
    fn set_status(&self, id: Uuid, status: DownloadStatus) {
        let old = {
            let mut items = self.items.lock();
            let Some(item) = items.iter_mut().find(|i| i.id == id) else { return };
            std::mem::replace(&mut item.status, status)
        };
        self.hub.publish(DownloadEvent::StatusChanged(DownloadStatusChanged {
            download_id: id,
            old_status: old,
            new_status: status,
            error_message: None,
        }));
    }
    fn report(&self, id: Uuid) -> Option<VirusTotalReport> {
        self.get(id).and_then(|i| i.virus_total)
    }
}

#[async_trait]
impl DownloadManager for FakeDownloads {
    fn downloads(&self) -> Vec<DownloadItem> {
        self.items.lock().clone()
    }
    fn get(&self, id: Uuid) -> Option<DownloadItem> {
        self.items.lock().iter().find(|i| i.id == id).cloned()
    }
    fn subscribe(&self) -> EventStream<DownloadEvent> {
        self.hub.subscribe()
    }
    fn subscribe_fn(&self, callback: EventCallback<DownloadEvent>) {
        self.hub.subscribe_fn(callback);
    }
    fn connections(&self, _id: Uuid) -> Vec<ConnectionInfo> {
        Vec::new()
    }
    async fn add_download(&self, _request: DownloadRequest) -> Result<DownloadItem> {
        unimplemented!()
    }
    async fn start(&self, id: Uuid) -> Result<()> {
        self.resume(id).await
    }
    fn pause(&self, id: Uuid) {
        self.paused.lock().push(id);
        self.set_status(id, DownloadStatus::Paused);
    }
    fn pause_all(&self) -> usize {
        0
    }
    async fn resume(&self, id: Uuid) -> Result<()> {
        self.resumed.lock().push(id);
        self.set_status(id, DownloadStatus::Downloading);
        Ok(())
    }
    fn stop(&self, _id: Uuid) {}
    async fn retry(&self, id: Uuid) -> Result<()> {
        self.resume(id).await
    }
    fn remove(&self, id: Uuid, delete_file: bool) {
        self.removed.lock().push((id, delete_file));
        let mut items = self.items.lock();
        let Some(index) = items.iter().position(|i| i.id == id) else { return };
        let item = items.remove(index);
        drop(items);
        self.hub.publish(DownloadEvent::ListChanged { change: DownloadListChangeType::Removed, item: Box::new(item) });
    }
    fn change_address(&self, _id: Uuid, _change: AddressChange) -> bool {
        false
    }
    fn control_connection(&self, _id: Uuid, _connection_id: i32, _command: ConnectionCommand) -> bool {
        false
    }
    async fn verify_checksum(&self, _id: Uuid, _e: ExpectedChecksum, _p: Option<ProgressFn<i64>>, _c: CancellationToken) -> Result<ChecksumResult> {
        Ok(ChecksumResult::NotChecked)
    }
    fn global_speed_limit(&self) -> i64 {
        0
    }
    fn set_speed_limit(&self, _id: Uuid, _bps: i64) {}
    fn set_completion_actions(&self, _id: Uuid, _a: CompletionActions, _p: PowerAction) {}
    fn set_queue_speed_limit(&self, _id: Uuid, _bps: i64) {}
    fn set_global_speed_limit(&self, _bps: i64) {}
    fn load_persisted_downloads(&self) {}
    async fn cleanup_orphaned_temp_folders(&self) -> usize {
        0
    }
    fn flush(&self) {}
    fn set_virus_total(&self, id: Uuid, report: VirusTotalReport) {
        let updated = {
            let mut items = self.items.lock();
            let Some(item) = items.iter_mut().find(|i| i.id == id) else { return };
            item.virus_total = Some(report);
            item.clone()
        };
        self.hub.publish(DownloadEvent::ItemUpdated(Box::new(updated)));
    }
}

// ---------------------------------------------------------------- fixture --

struct Fixture {
    api: Arc<FakeApi>,
    downloads: Arc<FakeDownloads>,
    settings: Arc<FakeSettings>,
    scanner: Arc<VirusTotalScanner>,
    events: EventStream<VirusTotalEvent>,
    folder: TempFolder,
}

impl Fixture {
    fn new(configure: impl FnOnce(&mut DownloadSettings)) -> Self {
        let mut settings =
            DownloadSettings { virus_total_enabled: true, virus_total_api_key: KEY.into(), ..Default::default() };
        configure(&mut settings);
        let settings = Arc::new(FakeSettings { current: Mutex::new(Arc::new(settings)), hub: EventHub::default() });
        let api = Arc::new(FakeApi::default());
        let downloads = Arc::new(FakeDownloads::default());
        let scanner = VirusTotalScanner::new(
            downloads.clone(),
            settings.clone(),
            api.clone(),
            None,
            Handle::current(),
            ScannerOptions { poll_interval: Duration::from_millis(1), url_polls: 3, file_polls: 3 },
        );
        let events = scanner.subscribe();
        Self { api, downloads, settings, scanner, events, folder: TempFolder::new() }
    }

    fn start(self) -> Self {
        self.scanner.start();
        self
    }

    /// A download of `hello` into the scratch folder, as the given status.
    fn item(&self, status: DownloadStatus) -> DownloadItem {
        DownloadItem {
            url: "https://files.example.com/tool.exe".into(),
            file_name: "tool.exe".into(),
            save_folder: self.folder.path_str(),
            status,
            ..Default::default()
        }
    }

    /// The file lands and the download completes.
    fn complete(&self, id: Uuid, bytes: &[u8]) {
        self.folder.write_file("tool.exe", bytes);
        self.downloads.set_status(id, DownloadStatus::Completed);
    }

    async fn wait_for(&self, what: &str, condition: impl Fn() -> bool) {
        for _ in 0..400 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for: {what}; calls were {:?}", self.api.calls());
    }

    async fn next_event(&self) -> VirusTotalEvent {
        for _ in 0..400 {
            if let Ok(event) = self.events.try_recv() {
                return event;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no VirusTotal event; calls were {:?}", self.api.calls());
    }

    async fn finished(&self, id: Uuid) -> VirusTotalReport {
        self.wait_for("the check to finish", || {
            self.downloads.report(id).is_some_and(|r| r.step == VirusTotalStep::Done) && !self.scanner.is_checking(id)
        })
        .await;
        self.downloads.report(id).unwrap()
    }
}

// ------------------------------------------------------------------ tests --

#[tokio::test]
async fn a_flagged_address_pauses_the_running_download_and_asks() {
    let fx = Fixture::new(|_| {}).start();
    *fx.api.url_report.lock() = result(4, 0, 60);

    let id = fx.downloads.add(fx.item(DownloadStatus::Downloading));

    let VirusTotalEvent::DecisionNeeded(notice) = fx.next_event().await else { panic!("expected a question") };
    assert_eq!(notice.download_id, id);
    assert!(!notice.finished);
    assert_eq!(notice.report.verdict, VirusTotalVerdict::Malicious);
    assert_eq!(notice.report.summary(), "Flagged as malicious by 4 of 69 security vendors");
    assert_eq!(*fx.downloads.paused.lock(), vec![id]);
    // Recorded before asking, so a restart asks again.
    assert!(fx.downloads.report(id).unwrap().awaits_decision());
    // Only the address was sent — nothing about a file that does not exist yet.
    assert_eq!(fx.api.calls(), vec!["url_report https://files.example.com/tool.exe"]);
}

#[tokio::test]
async fn continuing_resumes_the_download_and_records_the_choice() {
    let fx = Fixture::new(|_| {}).start();
    *fx.api.url_report.lock() = result(0, 2, 60);
    let id = fx.downloads.add(fx.item(DownloadStatus::Downloading));
    fx.next_event().await;

    fx.scanner.resolve(id, true);

    let report = fx.finished(id).await;
    assert!(report.kept_by_user);
    assert_eq!(report.verdict, VirusTotalVerdict::Suspicious);
    fx.wait_for("the download to resume", || fx.downloads.resumed.lock().contains(&id)).await;
    // Kept is final: the finished file is not checked and the user not asked twice.
    fx.complete(id, b"hello");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!fx.api.called("file_report"));
}

#[tokio::test]
async fn choosing_delete_removes_the_download_and_its_file() {
    let fx = Fixture::new(|_| {}).start();
    *fx.api.url_report.lock() = result(9, 0, 60);
    let id = fx.downloads.add(fx.item(DownloadStatus::Downloading));
    fx.next_event().await;

    fx.scanner.resolve(id, false);

    let VirusTotalEvent::Deleted { notice, automatically } = fx.next_event().await else { panic!("expected the deletion") };
    assert!(!automatically, "the user chose it");
    assert_eq!(notice.file_name, "tool.exe");
    assert_eq!(*fx.downloads.removed.lock(), vec![(id, true)]);
    fx.wait_for("the check to end", || !fx.scanner.is_checking(id)).await;
}

#[tokio::test]
async fn a_clean_address_waits_for_the_file_then_looks_up_its_hash() {
    let fx = Fixture::new(|_| {}).start();
    *fx.api.url_report.lock() = clean();
    *fx.api.file_report.lock() = clean();
    let id = fx.downloads.add(fx.item(DownloadStatus::Downloading));
    fx.wait_for("the address to be checked", || {
        fx.downloads.report(id).is_some_and(|r| r.step == VirusTotalStep::File)
    })
    .await;
    assert!(!fx.api.called("file_report"), "nothing about the file before it is complete");

    fx.complete(id, b"hello");

    let report = fx.finished(id).await;
    assert_eq!(report.verdict, VirusTotalVerdict::Clean);
    assert_eq!(report.source, Some(VirusTotalSource::File));
    assert_eq!(report.sha256.as_deref(), Some(HELLO_SHA256));
    assert_eq!(report.permalink, Some(file_link(HELLO_SHA256)));
    assert!(fx.api.called(&format!("file_report {HELLO_SHA256}")));
    assert!(!fx.api.called("upload"), "a file VirusTotal knows is not uploaded");
    assert!(fx.events.try_recv().is_err(), "a clean file needs nobody's attention");
}

#[tokio::test]
async fn an_address_virus_total_has_not_seen_is_submitted_and_its_analysis_awaited() {
    let fx = Fixture::new(|_| {}).start();
    *fx.api.url_analysis.lock() = result(1, 0, 60);
    let id = fx.downloads.add(fx.item(DownloadStatus::Downloading));

    let VirusTotalEvent::DecisionNeeded(notice) = fx.next_event().await else { panic!("expected a question") };
    assert_eq!(notice.report.source, Some(VirusTotalSource::Url));
    assert_eq!(notice.report.permalink, Some(url_link("object")));
    let calls = fx.api.calls();
    assert_eq!(calls[0], "url_report https://files.example.com/tool.exe");
    assert_eq!(calls[1], "submit_url https://files.example.com/tool.exe");
    assert_eq!(calls[2], "analysis url-analysis");
    assert_eq!(*fx.downloads.paused.lock(), vec![id]);
}

#[tokio::test]
async fn a_file_virus_total_has_not_seen_is_uploaded_and_a_bad_verdict_offers_keep_or_delete() {
    let fx = Fixture::new(|_| {}).start();
    *fx.api.url_report.lock() = clean();
    *fx.api.file_analysis.lock() = result(3, 0, 50);
    let id = fx.downloads.add(fx.item(DownloadStatus::Downloading));
    fx.wait_for("the address to be checked", || fx.downloads.report(id).is_some_and(|r| r.step == VirusTotalStep::File)).await;

    fx.complete(id, b"hello");

    let VirusTotalEvent::DecisionNeeded(notice) = fx.next_event().await else { panic!("expected a question") };
    assert!(notice.finished, "the file is already complete: keep or delete, not continue");
    assert_eq!(notice.report.source, Some(VirusTotalSource::File));
    assert!(fx.downloads.paused.lock().is_empty(), "a finished download is not paused");
    assert!(fx.api.called(&format!("upload {}", fx.folder.file("tool.exe"))));
}

#[tokio::test]
async fn a_file_over_the_upload_limit_is_only_looked_up() {
    let fx = Fixture::new(|s| s.virus_total_max_upload_megabytes = 1).start();
    *fx.api.url_report.lock() = clean();
    let id = fx.downloads.add(fx.item(DownloadStatus::Downloading));
    fx.wait_for("the address to be checked", || fx.downloads.report(id).is_some_and(|r| r.step == VirusTotalStep::File)).await;

    fx.complete(id, &vec![0u8; 1024 * 1024 + 1]);

    let report = fx.finished(id).await;
    assert!(fx.api.called("file_report"));
    assert!(!fx.api.called("upload"));
    // The address's verdict stands; the note says what was not done.
    assert_eq!(report.verdict, VirusTotalVerdict::Clean);
    assert!(report.note.as_deref().unwrap().contains("upload limit"), "{:?}", report.note);
}

#[tokio::test]
async fn the_upload_limit_is_never_more_than_25_mb() {
    let settings = DownloadSettings { virus_total_max_upload_megabytes: 500, ..Default::default() };
    assert_eq!(settings.virus_total_upload_limit_bytes(), 25 * 1024 * 1024);
}

#[tokio::test]
async fn a_private_download_is_only_ever_looked_up_by_its_hash() {
    let fx = Fixture::new(|_| {}).start();
    let item = DownloadItem { credentials: Some(DownloadCredentials::new("me", "secret")), ..fx.item(DownloadStatus::Downloading) };
    let id = fx.downloads.add(item);
    fx.wait_for("the check to wait for the file", || fx.downloads.report(id).is_some_and(|r| r.step == VirusTotalStep::File)).await;
    assert!(fx.api.calls().is_empty(), "not even the address: {:?}", fx.api.calls());

    fx.complete(id, b"hello");

    let report = fx.finished(id).await;
    assert_eq!(fx.api.calls(), vec![format!("file_report {HELLO_SHA256}")]);
    assert_eq!(report.verdict, VirusTotalVerdict::Unknown);
    assert!(report.note.as_deref().unwrap().contains("private source"), "{:?}", report.note);
}

#[tokio::test]
async fn a_private_download_is_shared_when_the_user_opted_in() {
    let fx = Fixture::new(|s| s.virus_total_upload_private_files = true).start();
    let mut item = fx.item(DownloadStatus::Downloading);
    item.headers.insert("Cookie".into(), "session=abc".into());
    let id = fx.downloads.add(item);
    fx.wait_for("the check to wait for the file", || fx.downloads.report(id).is_some_and(|r| r.step == VirusTotalStep::File)).await;

    fx.complete(id, b"hello");
    fx.finished(id).await;

    assert!(fx.api.called("url_report"));
    assert!(fx.api.called("upload"));
    // The cookie itself never appears in anything sent.
    assert!(fx.api.calls().iter().all(|c| !c.contains("session")));
}

#[tokio::test]
async fn a_login_in_the_address_is_stripped_before_it_is_sent() {
    let fx = Fixture::new(|s| s.virus_total_upload_private_files = true).start();
    let item = DownloadItem { url: "https://me:secret@files.example.com/tool.exe".into(), ..fx.item(DownloadStatus::Downloading) };
    fx.downloads.add(item);
    fx.wait_for("the address to be sent", || fx.api.called("url_report")).await;
    assert!(fx.api.calls().iter().all(|c| !c.contains("secret")), "{:?}", fx.api.calls());
}

#[tokio::test]
async fn warn_lets_the_download_carry_on() {
    let fx = Fixture::new(|s| s.virus_total_suspicious_action = VirusTotalAction::Warn).start();
    *fx.api.url_report.lock() = result(0, 1, 60);
    let id = fx.downloads.add(fx.item(DownloadStatus::Downloading));

    let VirusTotalEvent::Warned(notice) = fx.next_event().await else { panic!("expected a warning") };
    assert_eq!(notice.download_id, id);
    assert!(fx.downloads.paused.lock().is_empty());
    assert!(fx.downloads.removed.lock().is_empty());
    assert_eq!(fx.finished(id).await.verdict, VirusTotalVerdict::Suspicious);
}

#[tokio::test]
async fn delete_removes_without_asking() {
    let fx = Fixture::new(|s| s.virus_total_malicious_action = VirusTotalAction::Delete).start();
    *fx.api.url_report.lock() = result(5, 0, 60);
    let id = fx.downloads.add(fx.item(DownloadStatus::Downloading));

    assert!(matches!(fx.next_event().await, VirusTotalEvent::Deleted { automatically: true, .. }));
    assert_eq!(*fx.downloads.removed.lock(), vec![(id, true)]);
}

#[tokio::test]
async fn nothing_is_sent_while_the_check_is_off_or_has_no_key() {
    for configure in [
        (|s: &mut DownloadSettings| s.virus_total_enabled = false) as fn(&mut DownloadSettings),
        |s: &mut DownloadSettings| s.virus_total_api_key = "  ".into(),
    ] {
        let fx = Fixture::new(configure).start();
        let id = fx.downloads.add(fx.item(DownloadStatus::Downloading));
        fx.complete(id, b"hello");
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(fx.api.calls().is_empty());
        assert_eq!(fx.downloads.report(id), None);
    }
}

#[tokio::test]
async fn a_rejected_key_ends_the_check_and_says_why() {
    let fx = Fixture::new(|_| {}).start();
    fx.api.reject_key.store(true, Ordering::SeqCst);
    let id = fx.downloads.add(fx.item(DownloadStatus::Downloading));

    let report = fx.finished(id).await;
    assert_eq!(report.note.as_deref(), Some(KEY_REJECTED));
    assert_eq!(report.verdict, VirusTotalVerdict::Unknown);
    fx.complete(id, b"hello");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(fx.api.calls().len(), 1, "one refusal is enough: {:?}", fx.api.calls());
}

#[tokio::test]
async fn an_unanswered_question_is_asked_again_at_the_next_launch() {
    let fx = Fixture::new(|_| {});
    let mut item = fx.item(DownloadStatus::Paused);
    item.virus_total = Some(VirusTotalReport { verdict: VirusTotalVerdict::Malicious, malicious: 3, ..VirusTotalReport::started() });
    let id = item.id;
    fx.downloads.items.lock().push(item);

    let fx = fx.start();

    let VirusTotalEvent::DecisionNeeded(notice) = fx.next_event().await else { panic!("expected the question again") };
    assert_eq!(notice.download_id, id);
    assert!(fx.api.calls().is_empty(), "the verdict is already known");
    fx.scanner.resolve(id, true);
    fx.wait_for("the download to resume", || fx.downloads.resumed.lock().contains(&id)).await;
}

#[tokio::test]
async fn a_check_interrupted_while_waiting_for_the_file_carries_on_at_the_next_launch() {
    let fx = Fixture::new(|_| {});
    *fx.api.file_report.lock() = clean();
    let mut item = fx.item(DownloadStatus::Completed);
    item.virus_total = Some(VirusTotalReport { step: VirusTotalStep::File, ..VirusTotalReport::started() });
    let id = item.id;
    fx.folder.write_file("tool.exe", b"hello");
    fx.downloads.items.lock().push(item);

    let fx = fx.start();

    assert_eq!(fx.finished(id).await.verdict, VirusTotalVerdict::Clean);
    assert!(!fx.api.called("url_report"), "the address was already checked");
}

#[tokio::test]
async fn downloads_from_before_the_check_was_on_are_left_alone() {
    let fx = Fixture::new(|_| {});
    fx.downloads.items.lock().push(fx.item(DownloadStatus::Completed));
    let _fx = fx.start();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(_fx.api.calls().is_empty());
}

#[tokio::test]
async fn switching_the_check_on_picks_up_where_it_left_off() {
    let fx = Fixture::new(|s| s.virus_total_enabled = false);
    *fx.api.file_report.lock() = clean();
    let mut item = fx.item(DownloadStatus::Completed);
    item.virus_total = Some(VirusTotalReport { step: VirusTotalStep::File, ..VirusTotalReport::started() });
    let id = item.id;
    fx.folder.write_file("tool.exe", b"hello");
    fx.downloads.items.lock().push(item);
    let fx = fx.start();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(fx.api.calls().is_empty());

    let mut on = fx.settings.create_copy();
    on.virus_total_enabled = true;
    fx.settings.save(on);

    assert_eq!(fx.finished(id).await.verdict, VirusTotalVerdict::Clean);
}

#[tokio::test]
async fn removing_the_download_stops_its_check() {
    let fx = Fixture::new(|_| {}).start();
    *fx.api.url_report.lock() = clean();
    let id = fx.downloads.add(fx.item(DownloadStatus::Downloading));
    fx.wait_for("the check to wait for the file", || fx.downloads.report(id).is_some_and(|r| r.step == VirusTotalStep::File)).await;

    fx.downloads.remove(id, false);

    fx.wait_for("the check to end", || !fx.scanner.is_checking(id)).await;
    assert!(!fx.api.called("file_report"));
}

#[tokio::test]
async fn an_answer_with_no_check_waiting_still_stands() {
    // The check was switched off while the question was open, say.
    let fx = Fixture::new(|_| {}).start();
    let mut item = fx.item(DownloadStatus::Paused);
    item.virus_total = Some(VirusTotalReport { verdict: VirusTotalVerdict::Malicious, ..VirusTotalReport::started() });
    let id = item.id;
    fx.downloads.items.lock().push(item);

    fx.scanner.resolve(id, false);

    assert_eq!(*fx.downloads.removed.lock(), vec![(id, true)]);
}

#[tokio::test]
async fn malicious_votes_below_the_threshold_take_the_suspicious_action() {
    // The C# build's default: three vendors before it is called malicious.
    let fx = Fixture::new(|s| {
        s.virus_total_malicious_action = VirusTotalAction::Delete;
        s.virus_total_suspicious_action = VirusTotalAction::Warn;
    })
    .start();
    *fx.api.url_report.lock() = result(2, 0, 60);
    let id = fx.downloads.add(fx.item(DownloadStatus::Downloading));

    let VirusTotalEvent::Warned(notice) = fx.next_event().await else { panic!("expected a warning, not a deletion") };
    assert_eq!(notice.report.verdict, VirusTotalVerdict::Suspicious);
    assert_eq!(notice.report.malicious, 2);
    assert!(fx.downloads.removed.lock().is_empty());
    assert!(fx.downloads.get(id).is_some());
}

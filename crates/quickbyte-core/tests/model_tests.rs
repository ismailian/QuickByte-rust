//! Port of `tests/QuickByte.Core.Tests/Models/ModelTests.cs` and
//! `QueueScheduleTests.cs`: the models' own arithmetic and the rules their JSON
//! keeps.

use std::collections::BTreeMap;

use chrono::{NaiveDate, NaiveDateTime, TimeDelta, Weekday};
use quickbyte_core::enums::{DownloadCategory, DownloadStatus, PowerAction, ScheduleDays};
use quickbyte_core::helpers::file_category::{FileCategory, get_category};
use quickbyte_core::helpers::{app_paths, secret_protector};
use quickbyte_core::models::{
    CapturedDownload, DownloadCredentials, DownloadItem, DownloadQueue, DownloadSettings, Headers, QueueSchedule,
    RemoteFileInfo, RemoteFileVersion, RequestOptions, UpdateCheckResult, UpdateManifest, header_get, join_path,
};
use quickbyte_core::serde_compat::{DateTimeOffset, TimeSpan};

// 2026-08-26 is a Wednesday.
fn wednesday() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 8, 26).unwrap().and_hms_opt(0, 0, 0).unwrap()
}

fn hours(h: f64) -> TimeDelta {
    TimeDelta::milliseconds((h * 3_600_000.0) as i64)
}

fn minutes(m: i64) -> TimeDelta {
    TimeDelta::minutes(m)
}

fn days(d: i64) -> TimeDelta {
    TimeDelta::days(d)
}

/// A local wall-clock time as the `DateTimeOffset` a queue's `LastRunAt` holds.
fn local(dt: NaiveDateTime) -> DateTimeOffset {
    DateTimeOffset(dt.and_local_timezone(chrono::Local).single().expect("an unambiguous local time").fixed_offset())
}

// ---------------------------------------------------------------------------
// QueueSchedule: a window, not an instant, and every method takes the current
// time as a parameter — which is what lets the in-app scheduler and the agent
// reach the same verdict from the same file.
// ---------------------------------------------------------------------------

fn at(start: TimeSpan, days: ScheduleDays) -> QueueSchedule {
    QueueSchedule { enabled: true, days, start_time: start, ..Default::default() }
}

fn every_day(start: TimeSpan) -> QueueSchedule {
    at(start, ScheduleDays::EVERY_DAY)
}

fn span_hours(h: f64) -> TimeSpan {
    TimeSpan::from_secs_f64(h * 3600.0)
}

#[test]
fn a_disabled_schedule_is_never_in_a_window() {
    let mut schedule = every_day(span_hours(2.0));
    schedule.enabled = false;
    assert_eq!(schedule.window_start(wednesday() + hours(2.0)), None);
    assert_eq!(schedule.next_start(wednesday()), None);
}

#[test]
fn a_schedule_with_no_days_never_fires() {
    let schedule = at(span_hours(2.0), ScheduleDays::NONE);
    assert_eq!(schedule.window_start(wednesday() + hours(2.0)), None);
    assert_eq!(schedule.next_start(wednesday()), None);
}

#[test]
fn window_start_is_the_start_time_of_the_window_now_falls_in() {
    let schedule = every_day(span_hours(2.0));
    assert_eq!(schedule.window_start(wednesday() + hours(2.0) + minutes(30)), Some(wednesday() + hours(2.0)));
}

#[test]
fn window_start_is_none_before_the_start_time_arrives() {
    assert_eq!(every_day(span_hours(2.0)).window_start(wednesday() + hours(1.0)), None);
}

#[test]
fn a_missed_run_is_honoured_inside_the_grace_period() {
    // Signing in at 09:05 honours an 08:30 schedule.
    let schedule = every_day(span_hours(8.5));
    assert_eq!(schedule.window_start(wednesday() + hours(9.0) + minutes(5)), Some(wednesday() + hours(8.5)));
}

#[test]
fn a_missed_run_is_not_honoured_once_the_grace_period_has_passed() {
    // One at 11:00 does not. Without the bound, the first launch of the day
    // would start every schedule the week had missed.
    assert_eq!(every_day(span_hours(8.5)).window_start(wednesday() + hours(11.0)), None);
    assert_eq!(QueueSchedule::MISSED_RUN_GRACE, TimeSpan::from_hours(1));
}

fn overnight() -> QueueSchedule {
    QueueSchedule { stop_at_enabled: true, stop_time: span_hours(6.0), ..every_day(span_hours(22.0)) }
}

#[test]
fn a_window_may_cross_midnight() {
    // 22:00 until 06:00, written without a second date field. At 01:00 on
    // Thursday the run that matters is Wednesday's 22:00 one — Thursday's own
    // start time has not arrived yet.
    let thursday_early = wednesday() + days(1) + hours(1.0);
    assert_eq!(overnight().window_start(thursday_early), Some(wednesday() + hours(22.0)));
}

#[test]
fn a_window_that_crossed_midnight_closes_at_its_stop_time() {
    assert_eq!(overnight().window_start(wednesday() + days(1) + hours(7.0)), None);
}

#[test]
fn window_end_is_the_grace_period_when_no_stop_time_is_set() {
    let start = wednesday() + hours(2.0);
    assert_eq!(every_day(span_hours(2.0)).window_end(start), start + QueueSchedule::MISSED_RUN_GRACE.to_delta());
}

#[test]
fn window_end_pushes_a_stop_time_at_or_before_the_start_to_the_next_day() {
    assert_eq!(overnight().window_end(wednesday() + hours(22.0)), wednesday() + days(1) + hours(6.0));
}

#[test]
fn a_window_is_confined_to_the_days_it_runs_on() {
    // Wednesday is not a weekend day, and neither is the Tuesday before it.
    let schedule = at(span_hours(2.0), ScheduleDays::WEEKEND);
    assert_eq!(schedule.window_start(wednesday() + hours(2.0) + minutes(30)), None);
}

#[test]
fn next_start_is_later_today_when_the_time_has_not_come() {
    assert_eq!(every_day(span_hours(22.0)).next_start(wednesday() + hours(10.0)), Some(wednesday() + hours(22.0)));
}

#[test]
fn next_start_is_tomorrow_once_today_is_behind_us() {
    assert_eq!(every_day(span_hours(2.0)).next_start(wednesday() + hours(10.0)), Some(wednesday() + days(1) + hours(2.0)));
}

#[test]
fn next_start_looks_a_full_week_ahead() {
    // Today's occurrence is already behind us, so the same weekday a week out is
    // the answer — which is why the search runs to eight days, not seven.
    let schedule = at(span_hours(2.0), ScheduleDays::WEDNESDAY);
    assert_eq!(schedule.next_start(wednesday() + hours(10.0)), Some(wednesday() + days(7) + hours(2.0)));
}

#[test]
fn next_start_is_strictly_after_the_moment_it_is_given() {
    assert_eq!(every_day(span_hours(2.0)).next_start(wednesday() + hours(2.0)), Some(wednesday() + days(1) + hours(2.0)));
}

#[test]
fn to_flag_derives_the_flag_by_shifting_a_day_of_week() {
    assert_eq!(ScheduleDays::from_weekday(Weekday::Sun), ScheduleDays::SUNDAY);
    assert_eq!(ScheduleDays::from_weekday(Weekday::Mon), ScheduleDays::MONDAY);
    assert_eq!(ScheduleDays::from_weekday(Weekday::Sat), ScheduleDays::SATURDAY);
}

#[test]
fn runs_on_reads_the_flags() {
    let weekdays = at(TimeSpan::ZERO, ScheduleDays::WEEKDAYS);
    assert!(weekdays.runs_on(Weekday::Mon));
    assert!(weekdays.runs_on(Weekday::Fri));
    assert!(!weekdays.runs_on(Weekday::Sat));
    assert!(!weekdays.runs_on(Weekday::Sun));
}

#[test]
fn the_day_groups_add_up() {
    assert_eq!(ScheduleDays::EVERY_DAY, ScheduleDays::WEEKDAYS | ScheduleDays::WEEKEND);
    assert_eq!(ScheduleDays::WEEKEND, ScheduleDays::SATURDAY | ScheduleDays::SUNDAY);
    let all = every_day(TimeSpan::ZERO);
    for day in [Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri, Weekday::Sat, Weekday::Sun] {
        assert!(all.runs_on(day), "{day:?}");
    }
}

#[test]
fn clone_is_detached_from_the_schedule_the_runner_reads() {
    let original = QueueSchedule { stop_at_enabled: true, stop_time: span_hours(6.0), ..at(span_hours(2.0), ScheduleDays::WEEKDAYS) };
    let mut copy = original.clone();
    copy.enabled = false;
    copy.start_time = span_hours(23.0);
    copy.days = ScheduleDays::NONE;
    assert!(original.enabled);
    assert_eq!(original.start_time, span_hours(2.0));
    assert_eq!(original.days, ScheduleDays::WEEKDAYS);
}

#[test]
fn the_schedule_round_trips_in_the_c_sharp_shape() {
    let schedule = overnight();
    let json = serde_json::to_value(&schedule).unwrap();
    assert_eq!(json["Days"], 127);
    assert_eq!(json["StartTime"], "22:00:00");
    assert_eq!(json["StopTime"], "06:00:00");
    assert_eq!(serde_json::from_value::<QueueSchedule>(json).unwrap(), schedule);
}

// ---------------------------------------------------------------------------
// DownloadQueue
// ---------------------------------------------------------------------------

fn scheduled(start: TimeSpan) -> DownloadQueue {
    DownloadQueue { schedule: every_day(start), ..Default::default() }
}

#[test]
fn a_queue_in_its_window_that_has_never_run_is_due() {
    assert!(scheduled(span_hours(2.0)).is_due(wednesday() + hours(2.0) + minutes(10)));
}

#[test]
fn a_queue_already_started_in_this_window_is_not_due_again() {
    // The single question both the in-app scheduler and the agent ask, so two
    // watchers of one file cannot start the same window twice.
    let mut queue = scheduled(span_hours(2.0));
    queue.last_run_at = Some(local(wednesday() + hours(2.0) + minutes(1)));
    assert!(!queue.is_due(wednesday() + hours(2.0) + minutes(10)));
}

#[test]
fn a_run_from_a_previous_window_does_not_satisfy_this_one() {
    let mut queue = scheduled(span_hours(2.0));
    queue.last_run_at = Some(local(wednesday() - days(1) + hours(2.0)));
    assert!(queue.is_due(wednesday() + hours(2.0) + minutes(10)));
}

#[test]
fn next_run_at_is_now_when_a_run_is_owed() {
    assert_eq!(scheduled(span_hours(2.0)).next_run_at(wednesday() + hours(2.0) + minutes(10)), Some(wednesday() + hours(2.0)));
}

#[test]
fn next_run_at_is_the_next_occurrence_when_nothing_is_owed() {
    assert_eq!(scheduled(span_hours(2.0)).next_run_at(wednesday() + hours(10.0)), Some(wednesday() + days(1) + hours(2.0)));
}

#[test]
fn next_run_at_is_none_for_a_queue_that_is_not_scheduled() {
    assert_eq!(DownloadQueue::default().next_run_at(wednesday()), None);
}

#[test]
fn clamp_concurrency_keeps_the_queue_inside_its_own_bounds() {
    for (requested, expected) in [(0, 1), (-4, 1), (1, 1), (8, 8), (20, 20), (999, 20)] {
        let queue = DownloadQueue { concurrent_downloads: requested, ..Default::default() };
        assert_eq!(queue.clamp_concurrency(), expected, "{requested}");
    }
}

#[test]
fn an_explicit_null_in_the_json_does_not_beat_the_initializer() {
    let queue: DownloadQueue = serde_json::from_str(r#"{"Schedule":null,"ItemIds":null}"#).unwrap();
    assert_eq!(queue.schedule, QueueSchedule::default());
    assert!(queue.item_ids.is_empty());
}

#[test]
fn clone_gives_an_editor_a_copy_that_is_not_already_live() {
    let mut original =
        DownloadQueue { name: "Nightly".into(), concurrent_downloads: 3, when_finished: PowerAction::Sleep, ..Default::default() };
    original.item_ids.push(uuid::Uuid::new_v4());

    let mut copy = original.clone();
    assert_eq!(copy.when_finished, PowerAction::Sleep);
    copy.name = "Edited".into();
    copy.item_ids.push(uuid::Uuid::new_v4());
    copy.schedule.enabled = true;

    assert_eq!(original.name, "Nightly");
    assert_eq!(original.item_ids.len(), 1);
    assert!(!original.schedule.enabled);
    assert_eq!(original.id, copy.id);
}

#[test]
fn a_power_action_no_version_of_the_app_knows_reads_as_none_rather_than_failing_the_file() {
    let queue: DownloadQueue = serde_json::from_str(r#"{"WhenFinished":42}"#).unwrap();
    assert_eq!(queue.when_finished, PowerAction::None);
    let queue: DownloadQueue = serde_json::from_str(r#"{"WhenFinished":3}"#).unwrap();
    assert_eq!(queue.when_finished, PowerAction::Hibernate);
}

// ---------------------------------------------------------------------------
// DownloadItem
// ---------------------------------------------------------------------------

#[test]
fn progress_percentage_never_exceeds_a_full_bar() {
    for (done, total, expected) in [(0, 100, 0.0), (50, 100, 50.0), (100, 100, 100.0), (150, 100, 100.0), (50, 0, 0.0)] {
        let item = DownloadItem { downloaded_bytes: done, total_bytes: total, ..Default::default() };
        assert_eq!(item.progress_percentage(), expected, "{done}/{total}");
    }
}

#[test]
fn category_drives_which_sidebar_entry_a_row_belongs_to() {
    use DownloadStatus as S;
    for (status, expected) in [
        (S::Queued, DownloadCategory::Queued),
        (S::Connecting, DownloadCategory::InProgress),
        (S::Downloading, DownloadCategory::InProgress),
        (S::Merging, DownloadCategory::InProgress),
        (S::Verifying, DownloadCategory::InProgress),
        (S::Paused, DownloadCategory::Paused),
        (S::Completed, DownloadCategory::Completed),
        (S::Failed, DownloadCategory::Failed),
        (S::Cancelled, DownloadCategory::Failed),
    ] {
        assert_eq!(DownloadItem { status, ..Default::default() }.category(), expected, "{status:?}");
    }
}

#[test]
fn full_path_is_the_folder_and_the_name() {
    let item = DownloadItem { save_folder: r"C:\Downloads".into(), file_name: "a.bin".into(), ..Default::default() };
    assert_eq!(item.full_path(), join_path(r"C:\Downloads", "a.bin"));
    if cfg!(windows) {
        assert_eq!(item.full_path(), r"C:\Downloads\a.bin");
    }
}

#[test]
fn headers_survives_an_explicit_null_in_a_persisted_download() {
    let item: DownloadItem = serde_json::from_str(r#"{"Headers":null}"#).unwrap();
    assert!(item.headers.is_empty());
}

#[test]
fn to_request_options_leaves_headers_none_rather_than_empty() {
    let bare = DownloadItem::default().to_request_options();
    assert_eq!(bare.headers, None);
    assert!(!bare.has_headers());
    assert!(!bare.has_credentials());
}

#[test]
fn to_request_options_carries_the_login_and_the_captured_headers() {
    let mut item = DownloadItem { credentials: Some(DownloadCredentials::new("alice", "hunter2")), ..Default::default() };
    item.headers.insert("Cookie".into(), "session=abc".into());
    let options = item.to_request_options();
    assert!(options.has_credentials());
    assert!(options.has_headers());
    assert_eq!(options.headers.unwrap()["Cookie"], "session=abc");
}

#[test]
fn to_request_options_carries_what_the_probe_learned_about_the_cache() {
    // The flag is a fact about the server that the probe paid an extra request
    // to learn. A connection that does not carry it goes back to the cache and
    // gets the whole-file answer the probe worked around.
    assert!(DownloadItem { bypass_cache: true, ..Default::default() }.to_request_options().bypass_cache);
    assert!(!DownloadItem::default().to_request_options().bypass_cache);
}

#[test]
fn to_request_options_presents_the_version_only_to_a_server_that_proved_it_serves_ranges() {
    // A server that has not answered a range is fetched on one connection that
    // expects a 200. A condition would turn that into a false alarm.
    let version = RemoteFileVersion::new(Some("\"v1\"".into()), None, Some("example.com".into()));
    let proven = DownloadItem { supports_resume: true, remote_version: Some(version.clone()), ..Default::default() };
    let unproven = DownloadItem { supports_resume: false, remote_version: Some(version.clone()), ..Default::default() };
    assert_eq!(proven.to_request_options().resume_validator, Some(version));
    assert_eq!(unproven.to_request_options().resume_validator, None);
}

#[test]
fn to_request_options_stops_presenting_a_version_the_server_cannot_evaluate() {
    let item = DownloadItem {
        supports_resume: true,
        if_range_unsupported: true,
        remote_version: Some(RemoteFileVersion::new(Some("\"v1\"".into()), None, None)),
        ..Default::default()
    };
    assert_eq!(item.to_request_options().resume_validator, None);
}

#[test]
fn to_request_options_presents_no_version_that_has_no_validator_in_it() {
    let item = DownloadItem {
        supports_resume: true,
        remote_version: Some(RemoteFileVersion::new(None, None, Some("example.com".into()))),
        ..Default::default()
    };
    assert_eq!(item.to_request_options().resume_validator, None);
}

#[test]
fn a_version_is_tied_to_the_host_the_probe_ended_up_at() {
    let info = RemoteFileInfo {
        final_url: "https://objects.example.net:8443/f/setup.exe?sig=abc".into(),
        etag: Some("\"v1\"".into()),
        ..Default::default()
    };
    let version = info.version();
    assert_eq!(version.host.as_deref(), Some("objects.example.net:8443"));
    assert_eq!(version.etag.as_deref(), Some("\"v1\""));
}

#[test]
fn request_options_none_is_anonymous_and_header_free() {
    let none = RequestOptions::none();
    assert!(!none.has_credentials());
    assert!(!none.has_headers());
    assert!(!none.bypass_cache);
}

#[test]
fn with_cache_bypass_keeps_the_login_and_the_captured_headers() {
    // The escalated probe is the same request asked a different way. Losing the
    // credentials on the way would turn a 206 into a 401.
    let original = RequestOptions {
        credentials: Some(DownloadCredentials::new("alice", "hunter2")),
        headers: Some(Headers::from([("Cookie".to_string(), "session=abc".to_string())])),
        ..Default::default()
    };
    let bypassing = original.with_cache_bypass();
    assert!(bypassing.bypass_cache);
    assert_eq!(bypassing.credentials.as_ref().unwrap().user_name, "alice");
    assert_eq!(bypassing.resume_validator, None);
    assert_eq!(bypassing.headers.as_ref().unwrap()["Cookie"], "session=abc");
    assert!(!original.bypass_cache);
}

// ---------------------------------------------------------------------------
// DownloadCredentials and SecretProtector
// ---------------------------------------------------------------------------

#[test]
fn the_live_password_never_reaches_the_json() {
    let json = serde_json::to_string(&DownloadCredentials::new("alice", "hunter2")).unwrap();
    assert!(!json.contains("hunter2"));
    assert!(json.contains("alice"));
}

#[cfg(windows)]
#[test]
fn a_password_round_trips_through_the_protected_property() {
    // Same user, same machine: DPAPI gives it back.
    let json = serde_json::to_string(&DownloadCredentials::new("alice", "hunter2")).unwrap();
    let restored: DownloadCredentials = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.user_name, "alice");
    assert_eq!(restored.password, "hunter2");
}

#[test]
fn a_profile_copied_from_another_machine_loads_with_an_empty_password() {
    // Far better than a startup failure over one unreadable field: the download
    // simply asks again.
    let restored: DownloadCredentials =
        serde_json::from_str(r#"{"UserName":"alice","ProtectedPassword":"bm90IGEgcmVhbCBkcGFwaSBibG9i"}"#).unwrap();
    assert_eq!(restored.user_name, "alice");
    assert_eq!(restored.password, "");
}

#[test]
fn is_empty_is_what_decides_whether_anything_is_presented() {
    assert!(DownloadCredentials::default().is_empty());
    assert!(!DownloadCredentials::new("alice", "").is_empty());
    assert!(!DownloadCredentials::new("", "hunter2").is_empty());
}

#[test]
fn clone_copies_the_live_password() {
    let clone = DownloadCredentials::new("alice", "hunter2").clone();
    assert_eq!((clone.user_name.as_str(), clone.password.as_str()), ("alice", "hunter2"));
}

#[cfg(windows)]
#[test]
fn a_secret_round_trips_for_the_user_that_wrote_it() {
    let protected = secret_protector::protect("hunter2").unwrap();
    assert_ne!(protected, "hunter2");
    assert_eq!(secret_protector::unprotect(Some(&protected)), "hunter2");
}

#[test]
fn nothing_to_protect_protects_to_nothing() {
    assert_eq!(secret_protector::protect(""), None);
}

#[test]
fn anything_unreadable_comes_back_empty_rather_than_failing() {
    for stored in [None, Some(""), Some("not base64 at all !!"), Some("dHJ1bmNhdGVk")] {
        assert_eq!(secret_protector::unprotect(stored), "", "{stored:?}");
    }
}

#[cfg(windows)]
#[test]
fn the_ciphertext_is_bound_to_this_application() {
    // App-specific entropy, so a blob lifted out of QuickByte's file cannot be
    // handed to another CurrentUser-scope DPAPI consumer to decrypt.
    use base64::Engine;
    let mine = secret_protector::protect("hunter2").unwrap();
    let cipher = base64::engine::general_purpose::STANDARD.decode(mine).unwrap();
    let raw = quickbyte_core::platform::dpapi::unprotect(&cipher, b"QuickByte.Credential.v1").unwrap();
    assert_eq!(raw, b"hunter2");
    assert_eq!(quickbyte_core::platform::dpapi::unprotect(&cipher, b"some other app"), None);
}

// ---------------------------------------------------------------------------
// DownloadSettings
// ---------------------------------------------------------------------------

#[test]
fn clamp_connections_keeps_a_request_inside_the_supported_range() {
    for (requested, expected) in [(0, 1), (-3, 1), (1, 1), (8, 8), (32, 32), (500, 32)] {
        assert_eq!(DownloadSettings::default().clamp_connections(requested), expected);
    }
}

#[test]
fn clamp_buffer_size_defuses_a_hand_edited_settings_file() {
    // StreamBufferSizeBytes has no field in Options, so the only way it holds a
    // number is by being edited into settings.json. A 0 there would reach a
    // buffer allocation and a file write.
    let min = DownloadSettings::MIN_BUFFER_SIZE_BYTES as usize;
    let max = DownloadSettings::MAX_BUFFER_SIZE_BYTES as usize;
    for (configured, expected) in [(81920, 81920), (0, min), (-1, min), (64, min), (i32::MAX, max)] {
        let settings = DownloadSettings { stream_buffer_size_bytes: configured, ..Default::default() };
        assert_eq!(settings.clamp_buffer_size(), expected);
    }
}

fn sorted(chosen: &[(FileCategory, &str)]) -> DownloadSettings {
    DownloadSettings {
        default_download_folder: r"C:\Users\a\Downloads\QuickByte".into(),
        sort_into_category_folders: true,
        category_folders: chosen.iter().map(|(c, f)| (*c, f.to_string())).collect(),
        ..Default::default()
    }
}

#[test]
fn without_sorting_every_file_goes_to_the_default_folder() {
    let mut settings = sorted(&[(FileCategory::Video, r"D:\Videos")]);
    settings.sort_into_category_folders = false;
    assert_eq!(settings.folder_for("film.mkv"), r"C:\Users\a\Downloads\QuickByte");
}

#[cfg(windows)]
#[test]
fn sorting_puts_a_file_in_a_subfolder_named_after_its_kind() {
    assert_eq!(sorted(&[]).folder_for("setup.EXE"), r"C:\Users\a\Downloads\QuickByte\Programs");
    assert_eq!(sorted(&[]).folder_for("archive.7z"), r"C:\Users\a\Downloads\QuickByte\Compressed");
}

#[test]
fn a_file_of_no_known_kind_stays_in_the_default_folder() {
    // A subfolder called "Other" is not a place anyone looks.
    assert_eq!(sorted(&[]).folder_for("notes.xyz"), r"C:\Users\a\Downloads\QuickByte");
    assert_eq!(sorted(&[]).folder_for("download"), r"C:\Users\a\Downloads\QuickByte");
}

#[test]
fn a_folder_chosen_for_a_kind_wins_over_the_subfolder() {
    assert_eq!(sorted(&[(FileCategory::Video, r"D:\Media\Videos")]).folder_for("film.mp4"), r"D:\Media\Videos");
}

#[cfg(windows)]
#[test]
fn a_chosen_folder_that_is_not_a_full_path_is_ignored() {
    // settings.json is editable, and a relative path would resolve against
    // wherever QuickByte happened to be started from.
    for chosen in [r"..\..\Windows", "Videos", r"~\Videos", "C:Videos", "   "] {
        assert_eq!(sorted(&[(FileCategory::Video, chosen)]).folder_for("film.mp4"), r"C:\Users\a\Downloads\QuickByte\Video", "{chosen}");
    }
}

#[test]
fn a_null_category_map_in_a_hand_edited_file_reads_as_empty() {
    let settings: DownloadSettings = serde_json::from_str(r#"{"CategoryFolders":null}"#).unwrap();
    assert!(settings.category_folders.is_empty());
}

#[test]
fn the_defaults_are_the_ones_the_app_is_documented_with() {
    let settings = DownloadSettings::default();
    assert_eq!(settings.default_connections_count, 8);
    assert_eq!(settings.progress_update_interval_milliseconds, 100);
    assert_eq!(settings.max_concurrent_downloads, 3);
    assert_eq!(settings.global_speed_limit_bytes_per_second, 0);
    assert_eq!(settings.browser_integration_port, 9614);
    // A fixed default would be no secret at all — every install would accept
    // every install's extension.
    assert_eq!(settings.browser_integration_token, "");
}

#[test]
fn the_chunk_folder_lives_beside_the_rest_of_the_app_state() {
    // It used to be %TEMP%\QuickByte, and %TEMP% is emptied on a schedule — while
    // resume is driven entirely by chunk length on disk, so a sweep during a
    // pause does not lose a cache, it loses the download. (Only the path is
    // computed here; nothing under the real profile is touched.)
    assert_eq!(DownloadSettings::default().temp_folder, app_paths::data().join("temp").to_string_lossy());
}

#[test]
fn where_finished_files_land_is_still_the_users_own_folder() {
    // Only the chunk folder moved.
    let profile = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    assert_eq!(DownloadSettings::default().default_download_folder, join_path(&join_path(&profile, "Downloads"), "QuickByte"));
}

#[test]
fn the_chunk_folder_is_not_persisted_so_an_existing_install_is_carried_over() {
    let json = serde_json::to_string(&DownloadSettings::default()).unwrap();
    assert!(!json.contains("TempFolder"));
    assert!(json.contains("DefaultDownloadFolder"));

    let loaded: DownloadSettings =
        serde_json::from_str(r#"{"DefaultDownloadFolder":"D:\\keep-me","TempFolder":"C:\\Windows\\Temp\\QuickByte"}"#).unwrap();
    assert_eq!(loaded.default_download_folder, r"D:\keep-me");
    assert_eq!(loaded.temp_folder, DownloadSettings::default().temp_folder);
}

#[test]
fn the_connection_choices_are_the_ones_the_drop_downs_offer() {
    assert_eq!(DownloadSettings::CONNECTION_CHOICES, [1, 2, 4, 8, 16, 24, 32]);
    // The default has to be one of them, or Options opens on a value it cannot
    // select and reads back as the first entry.
    assert!(DownloadSettings::CONNECTION_CHOICES.contains(&DownloadSettings::DEFAULT_CONNECTIONS));
    assert_eq!(DownloadSettings::MIN_CONNECTIONS, DownloadSettings::CONNECTION_CHOICES[0]);
    assert_eq!(DownloadSettings::MAX_CONNECTIONS, DownloadSettings::CONNECTION_CHOICES[6]);
}

#[test]
fn nearest_connection_choice_snaps_down_never_up() {
    for (requested, expected) in [(8, 8), (1, 1), (32, 32), (5, 4), (3, 2), (31, 24), (0, 1), (-4, 1), (64, 32)] {
        assert_eq!(DownloadSettings::nearest_connection_choice(requested), expected, "{requested}");
    }
}

#[test]
fn category_folders_are_keyed_by_name() {
    let settings = DownloadSettings {
        category_folders: BTreeMap::from([(FileCategory::Video, r"D:\Films".to_string())]),
        ..Default::default()
    };
    let json = serde_json::to_value(&settings).unwrap();
    assert_eq!(json["CategoryFolders"]["Video"], r"D:\Films");
}

// ---------------------------------------------------------------------------
// CapturedDownload
// ---------------------------------------------------------------------------

#[test]
fn to_headers_carries_what_makes_a_signed_in_link_resolve() {
    let headers = CapturedDownload {
        url: "https://example.com/f.bin".into(),
        cookie: Some("session=abc".into()),
        referrer: Some("https://example.com/page".into()),
        user_agent: Some("Mozilla/5.0".into()),
        ..Default::default()
    }
    .to_headers();
    assert_eq!(headers["Cookie"], "session=abc");
    assert_eq!(headers["Referer"], "https://example.com/page");
    assert_eq!(headers["User-Agent"], "Mozilla/5.0");
}

#[test]
fn to_headers_drops_an_empty_value_rather_than_sending_it_blank() {
    // An empty Referer is a different request from no Referer at all.
    let headers = CapturedDownload { cookie: Some("  ".into()), referrer: Some(String::new()), user_agent: None, ..Default::default() }
        .to_headers();
    assert!(headers.is_empty());
}

#[test]
fn to_headers_is_looked_up_case_insensitively_like_the_wire_it_came_off() {
    let headers = CapturedDownload { cookie: Some("session=abc".into()), ..Default::default() }.to_headers();
    assert_eq!(header_get(&headers, "cookie"), Some("session=abc"));
}

// ---------------------------------------------------------------------------
// The updater's models
// ---------------------------------------------------------------------------

#[test]
fn a_manifest_needs_a_version_and_a_link_to_be_worth_acting_on() {
    let m = |version: &str, url: &str| UpdateManifest { version: version.into(), download_url: url.into(), ..Default::default() };
    assert!(m("1.4.0", "https://x/y.exe").is_usable());
    assert!(!m("1.4.0", "").is_usable());
    assert!(!m("", "https://x/y.exe").is_usable());
    assert!(!UpdateManifest::default().is_usable());
}

#[test]
fn a_manifest_binds_whichever_way_it_was_hand_written() {
    let manifest: UpdateManifest = serde_json::from_str(r#"{"Version":"1.4.0","downloadUrl":"https://x/y.exe"}"#).unwrap();
    assert_eq!(manifest.version, "1.4.0");
    assert_eq!(manifest.download_url, "https://x/y.exe");
}

#[test]
fn the_optional_parts_of_a_manifest_really_are_optional() {
    let manifest: UpdateManifest = serde_json::from_str(r#"{"version":"1.4.0","downloadUrl":"https://x/y.exe"}"#).unwrap();
    assert!(manifest.is_usable());
    assert_eq!(manifest.release_notes, None);
    assert_eq!(manifest.sha256, None);
    assert_eq!(manifest.release_date, None);
    assert_eq!(manifest.file_size_bytes, 0);
}

#[test]
fn an_up_to_date_result_still_has_a_number_to_show() {
    let result = UpdateCheckResult::up_to_date("1.4.0", None);
    assert!(!result.update_available);
    assert_eq!(result.current_version, "1.4.0");
    assert_eq!(result.latest_version(), "1.4.0");
    assert_eq!(result.manifest, None);
}

#[test]
fn an_available_result_carries_both_versions() {
    let manifest = UpdateManifest { version: "1.5.0".into(), download_url: "https://x/y.exe".into(), ..Default::default() };
    let result = UpdateCheckResult::available("1.4.0", manifest.clone());
    assert!(result.update_available);
    assert_eq!(result.current_version, "1.4.0");
    assert_eq!(result.latest_version(), "1.5.0");
    assert_eq!(result.manifest, Some(manifest));
}

// ---------------------------------------------------------------------------
// FileCategoryHelper
// ---------------------------------------------------------------------------

#[test]
fn get_category_reads_the_extension() {
    for (name, expected) in [
        ("archive.zip", FileCategory::Compressed),
        ("archive.7z", FileCategory::Compressed),
        ("paper.PDF", FileCategory::Documents),
        ("song.mp3", FileCategory::Music),
        ("setup.exe", FileCategory::Programs),
        ("app.apk", FileCategory::Programs),
        ("movie.mkv", FileCategory::Video),
        ("photo.jpeg", FileCategory::Pictures),
    ] {
        assert_eq!(get_category(name), expected, "{name}");
    }
}

#[test]
fn anything_unrecognised_is_other() {
    for name in ["mystery.qqq", "noextension", ""] {
        assert_eq!(get_category(name), FileCategory::Other, "{name}");
    }
}

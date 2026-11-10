//! The last step, and the one where a mistake is permanent: each chunk is
//! deleted as soon as its bytes are in the output, so a merge that concatenates
//! them in the wrong order — or resumes from the wrong place after an
//! interruption — produces a corrupt file with nothing left to rebuild it from.
//!
//! Most of the chunks here sit on the same volume as the destination, which is
//! the in-place path: appended onto the first chunk, then renamed. The copy path
//! a second volume takes is forced with the merger's own seam, since a test
//! machine cannot be relied on to have two. (`FileMergerTests.cs`.)

use std::sync::Arc;

use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::helpers::disk_space::scratch::TempFolder;
use crate::traits::FileMerger as _;

const BUFFER_SIZE: usize = 4096;

fn in_place(free: Option<i64>) -> FileMerger {
    FileMerger::with_disk(Arc::new(move |_| free), Arc::new(|_, _| true))
}

fn copying(free: Option<i64>) -> FileMerger {
    FileMerger::with_disk(Arc::new(move |_| free), Arc::new(|_, _| false))
}

fn merger(in_place_: bool) -> FileMerger {
    if in_place_ { in_place(None) } else { copying(None) }
}

fn out(folder: &TempFolder) -> String {
    Path::new(&folder.path_str()).join("out").join("merged.bin").to_string_lossy().into_owned()
}

fn plan(destination: &str, in_place_: bool, chunks: &[(&str, i64)]) -> Plan {
    Plan { destination: destination.into(), in_place: in_place_, chunks: chunks.iter().map(|&(n, l)| PlanChunk::new(n, l)).collect() }
}

async fn merge(m: &FileMerger, folder: &str, chunks: &[String], destination: &str) -> Result<()> {
    m.merge(folder, chunks, destination, BUFFER_SIZE, None, &CancellationToken::new()).await
}

// ------------------------------------------------------------ what it makes --

#[tokio::test]
async fn chunks_are_concatenated_in_the_order_they_are_given() {
    for way in [true, false] {
        let folder = TempFolder::new();
        let a = folder.write_file("part0.tmp", &[1, 2, 3]);
        let b = folder.write_file("part1.tmp", &[4, 5]);
        let c = folder.write_file("part2.tmp", &[6]);
        merge(&merger(way), &folder.path_str(), &[a, b, c], &out(&folder)).await.unwrap();
        assert_eq!(fs::read(out(&folder)).unwrap(), [1, 2, 3, 4, 5, 6]);
    }
}

#[tokio::test]
async fn a_merge_spans_more_than_one_buffer() {
    for way in [true, false] {
        let folder = TempFolder::new();
        let first: Vec<u8> = (0..10_000).map(|i| (i % 251) as u8).collect();
        let second: Vec<u8> = (0..7_000).map(|i| (i % 253) as u8).collect();
        let a = folder.write_file("part0.tmp", &first);
        let b = folder.write_file("part1.tmp", &second);
        merge(&merger(way), &folder.path_str(), &[a, b], &out(&folder)).await.unwrap();
        assert_eq!(fs::read(out(&folder)).unwrap(), [first, second].concat());
    }
}

#[tokio::test]
async fn an_empty_chunk_contributes_nothing_and_breaks_nothing() {
    let folder = TempFolder::new();
    let a = folder.write_file("part0.tmp", &[1, 2]);
    let empty = folder.write_file("part1.tmp", &[]);
    let c = folder.write_file("part2.tmp", &[3]);
    merge(&in_place(None), &folder.path_str(), &[a, empty, c], &out(&folder)).await.unwrap();
    assert_eq!(fs::read(out(&folder)).unwrap(), [1, 2, 3]);
}

#[tokio::test]
async fn merging_overwrites_a_file_that_is_already_there() {
    for way in [true, false] {
        let folder = TempFolder::new();
        let a = folder.write_file("part0.tmp", &[9]);
        fs::create_dir_all(folder.file("out")).unwrap();
        fs::write(out(&folder), [1, 2, 3, 4, 5]).unwrap();
        merge(&merger(way), &folder.path_str(), &[a], &out(&folder)).await.unwrap();
        assert_eq!(fs::read(out(&folder)).unwrap(), [9]);
    }
}

#[tokio::test]
async fn the_destination_folder_is_created_if_it_is_missing() {
    let folder = TempFolder::new();
    let a = folder.write_file("part0.tmp", &[1]);
    let destination = Path::new(&folder.path_str()).join("a").join("b").join("merged.bin").to_string_lossy().into_owned();
    merge(&in_place(None), &folder.path_str(), &[a], &destination).await.unwrap();
    assert!(Path::new(&destination).exists());
}

// ------------------------------------------------------------- disk space --

#[tokio::test]
async fn every_chunk_is_gone_once_its_bytes_are_in_the_file() {
    // Deleting them all at the end is what made a download need twice its size
    // in free space for the length of the merge.
    for way in [true, false] {
        let folder = TempFolder::new();
        let a = folder.write_file("part0.tmp", &[1]);
        let b = folder.write_file("part1.tmp", &[2]);
        merge(&merger(way), &folder.path_str(), &[a.clone(), b.clone()], &out(&folder)).await.unwrap();
        assert!(!Path::new(&a).exists());
        assert!(!Path::new(&b).exists());
        assert!(!Path::new(&folder.file(PLAN_FILE_NAME)).exists());
    }
}

#[tokio::test]
async fn a_single_chunk_on_the_same_volume_is_renamed_rather_than_copied() {
    // A one-connection download of 40 GB used to spend minutes "merging" by
    // copying every byte. A move keeps the file's identity: its creation time
    // (C#'s tell) and here, more directly, the file it was.
    let folder = TempFolder::new();
    let a = folder.write_file("part0.tmp", &[1, 2, 3]);
    let created = fs::metadata(&a).unwrap().created().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    merge(&in_place(None), &folder.path_str(), &[a], &out(&folder)).await.unwrap();
    assert_eq!(fs::metadata(out(&folder)).unwrap().created().unwrap(), created);
}

#[tokio::test]
async fn a_merge_in_place_needs_room_only_for_the_largest_chunk_it_appends() {
    let folder = TempFolder::new();
    let a = folder.write_file("part0.tmp", &[0; 900]);
    let b = folder.write_file("part1.tmp", &[0; 100]);
    merge(&in_place(Some(100)), &folder.path_str(), &[a, b], &out(&folder)).await.unwrap();
    assert_eq!(fs::metadata(out(&folder)).unwrap().len(), 1000);
}

#[tokio::test]
async fn a_merge_that_will_not_fit_says_so_before_touching_anything() {
    let folder = TempFolder::new();
    let a = folder.write_file("part0.tmp", &[1, 2, 3]);
    let b = folder.write_file("part1.tmp", &[4, 5]);
    let error = merge(&copying(Some(4)), &folder.path_str(), &[a.clone(), b.clone()], &out(&folder)).await.unwrap_err();
    let CoreError::InsufficientDiskSpace(shortfall) = error else { panic!("{error:?}") };
    assert_eq!(shortfall.needed, 5);
    assert_eq!(fs::read(&a).unwrap(), [1, 2, 3]);
    assert!(Path::new(&b).exists());
    assert!(!Path::new(&folder.file(PLAN_FILE_NAME)).exists());
}

// ------------------------------------------------------- an interruption --

#[tokio::test]
async fn a_merge_cancelled_half_way_through_a_chunk_resumes_into_the_right_file() {
    // The app exiting in the middle of a big merge. Chunks already merged are
    // gone, one is half appended, and the rest are untouched — only the plan and
    // the output's length say how far it got.
    for way in [true, false] {
        let folder = TempFolder::new();
        let parts: Vec<Vec<u8>> = (0..3).map(|p| (0..5 * 1024 * 1024).map(|i| ((i + p * 7) % 251) as u8).collect()).collect();
        let paths: Vec<String> = parts.iter().enumerate().map(|(i, b)| folder.write_file(&format!("part{i}.tmp"), b)).collect();

        let cancel = CancellationToken::new();
        let on_report = cancel.clone();
        let progress: ProgressFn<i64> = Arc::new(move |merged| {
            if merged > 5 * 1024 * 1024 + 1 {
                on_report.cancel();
            }
        });
        let error = merger(way).merge(&folder.path_str(), &paths, &out(&folder), 64 * 1024, Some(progress), &cancel).await.unwrap_err();
        assert!(error.is_cancelled());
        assert!(merger(way).has_interrupted_merge(&folder.path_str()));

        // Resumed the way DownloadService resumes one: no chunk list, the plan decides.
        merger(way).merge(&folder.path_str(), &[], &out(&folder), 64 * 1024, None, &CancellationToken::new()).await.unwrap();
        assert_eq!(fs::read(out(&folder)).unwrap(), parts.concat());
        assert!(!merger(way).has_interrupted_merge(&folder.path_str()));
    }
}

#[tokio::test]
async fn a_chunk_whose_delete_was_interrupted_is_not_merged_twice() {
    // Flushed into the output, then the process died before the delete.
    let folder = TempFolder::new();
    folder.write_file("part0.tmp", &[1, 2, 3, 4, 5]);
    folder.write_file("part1.tmp", &[4, 5]);
    folder.write_file("part2.tmp", &[6]);
    write_plan(&folder.path_str(), &plan(&out(&folder), true, &[("part0.tmp", 3), ("part1.tmp", 2), ("part2.tmp", 1)])).unwrap();

    merge(&in_place(None), &folder.path_str(), &[], &out(&folder)).await.unwrap();
    assert_eq!(fs::read(out(&folder)).unwrap(), [1, 2, 3, 4, 5, 6]);
    assert!(!Path::new(&folder.file("part1.tmp")).exists());
}

#[tokio::test]
async fn a_merge_interrupted_after_its_rename_just_finishes() {
    let folder = TempFolder::new();
    fs::create_dir_all(folder.file("out")).unwrap();
    fs::write(out(&folder), [1, 2, 3]).unwrap();
    write_plan(&folder.path_str(), &plan(&out(&folder), true, &[("part0.tmp", 2), ("part1.tmp", 1)])).unwrap();

    merge(&in_place(None), &folder.path_str(), &[], &out(&folder)).await.unwrap();
    assert_eq!(fs::read(out(&folder)).unwrap(), [1, 2, 3]);
    assert!(!in_place(None).has_interrupted_merge(&folder.path_str()));
}

#[tokio::test]
async fn a_chunk_missing_from_a_resumed_merge_is_reported_and_the_plan_forgotten() {
    // Nothing to rebuild from. Saying so, and dropping the plan so the next run
    // downloads what is missing, beats failing identically forever.
    let folder = TempFolder::new();
    folder.write_file("part0.tmp", &[1, 2]);
    write_plan(&folder.path_str(), &plan(&out(&folder), true, &[("part0.tmp", 2), ("part1.tmp", 1)])).unwrap();

    let error = merge(&in_place(None), &folder.path_str(), &[], &out(&folder)).await.unwrap_err();
    assert!(matches!(&error, CoreError::Io(m) if m.contains("downloaded again")), "{error:?}");
    assert!(!in_place(None).has_interrupted_merge(&folder.path_str()));
    assert!(!Path::new(&out(&folder)).exists());
}

#[tokio::test]
async fn a_plan_that_cannot_be_read_is_never_merged_into_an_empty_file() {
    let folder = TempFolder::new();
    folder.write_file(PLAN_FILE_NAME, b"{ not a plan");
    let error = merge(&in_place(None), &folder.path_str(), &[], &out(&folder)).await.unwrap_err();
    assert!(matches!(error, CoreError::Io(_)));
    assert!(!Path::new(&out(&folder)).exists());
}

#[tokio::test]
async fn a_plan_that_names_a_file_outside_its_folder_is_not_believed() {
    let folder = TempFolder::new();
    let outside = folder.write_file("outside.bin", &[7]);
    let temp = folder.file("temp");
    write_plan(&temp, &plan(&out(&folder), false, &[(r"..\outside.bin", 1)])).unwrap();

    let error = merge(&copying(None), &temp, &[], &out(&folder)).await.unwrap_err();
    assert!(matches!(error, CoreError::Io(_)));
    assert!(Path::new(&outside).exists());
}

#[test]
fn discarding_an_interrupted_copy_removes_the_half_file_it_was_writing() {
    let folder = TempFolder::new();
    fs::create_dir_all(folder.file("out")).unwrap();
    fs::write(out(&folder), [1]).unwrap();
    write_plan(&folder.path_str(), &plan(&out(&folder), false, &[("part0.tmp", 1), ("part1.tmp", 1)])).unwrap();

    copying(None).discard_interrupted_merge(&folder.path_str(), &out(&folder));
    assert!(!Path::new(&out(&folder)).exists());
    assert!(!copying(None).has_interrupted_merge(&folder.path_str()));
}

#[test]
fn discarding_never_deletes_a_file_the_download_does_not_name() {
    // The plan is read from disk; a path in it is not one to delete on its word alone.
    let folder = TempFolder::new();
    let someone_elses = folder.write_file("important.docx", &[1]);
    write_plan(&folder.path_str(), &plan(&someone_elses, false, &[("part0.tmp", 1)])).unwrap();

    copying(None).discard_interrupted_merge(&folder.path_str(), &out(&folder));
    assert!(Path::new(&someone_elses).exists());
}

#[test]
fn the_plan_is_written_in_the_c_sharp_schema() {
    let folder = TempFolder::new();
    write_plan(&folder.path_str(), &plan(r"D:\Films\a.mkv", true, &[("part0.tmp", 5), ("part1.tmp", 3)])).unwrap();
    assert_eq!(
        fs::read_to_string(folder.file(PLAN_FILE_NAME)).unwrap(),
        r#"{"Destination":"D:\\Films\\a.mkv","InPlace":true,"Chunks":[{"Name":"part0.tmp","Length":5},{"Name":"part1.tmp","Length":3}]}"#
    );
    // What a C# build left, escaped as System.Text.Json escapes.
    folder.write_file(PLAN_FILE_NAME, br#"{"Destination":"D:\\Fl\u00FCgel\\a.mkv","InPlace":false,"Chunks":[{"Name":"part0.tmp","Length":5}]}"#);
    assert_eq!(read_plan(&folder.path_str()), Some(plan("D:\\Flügel\\a.mkv", false, &[("part0.tmp", 5)])));
}

// --------------------------------------------------------------- progress --

#[tokio::test]
async fn progress_ends_on_the_exact_total() {
    // The throttle can swallow the last report, and a bar that stops at 99.4%
    // reads as a stall.
    let folder = TempFolder::new();
    let a = folder.write_file("part0.tmp", &[0; 5_000]);
    let b = folder.write_file("part1.tmp", &[0; 3_000]);
    let reported = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&reported);
    let progress: ProgressFn<i64> = Arc::new(move |m| sink.lock().push(m));

    in_place(None).merge(&folder.path_str(), &[a, b], &out(&folder), BUFFER_SIZE, Some(progress), &CancellationToken::new()).await.unwrap();
    assert_eq!(*reported.lock().last().unwrap(), 8_000);
}

#[tokio::test]
async fn a_cancelled_merge_stops() {
    let folder = TempFolder::new();
    let a = folder.write_file("part0.tmp", &[0; 1024]);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = in_place(None).merge(&folder.path_str(), &[a], &folder.file("merged.bin"), BUFFER_SIZE, None, &cancel).await.unwrap_err();
    assert!(error.is_cancelled());
}

// ---------------------------------------------------------------- cleanup --

#[test]
fn cleanup_chunks_removes_the_chunks_and_then_the_folder() {
    let parent = TempFolder::new();
    let chunks = parent.file("chunks");
    fs::create_dir_all(&chunks).unwrap();
    let a = Path::new(&chunks).join("part0.tmp").to_string_lossy().into_owned();
    let b = Path::new(&chunks).join("part1.tmp").to_string_lossy().into_owned();
    fs::write(&a, [1]).unwrap();
    fs::write(&b, [2]).unwrap();

    FileMerger::new().cleanup_chunks(&[a, b], &chunks);
    assert!(!Path::new(&chunks).exists());
}

#[test]
fn cleanup_chunks_leaves_a_folder_that_still_holds_something_else() {
    let parent = TempFolder::new();
    let chunks = parent.file("chunks");
    fs::create_dir_all(&chunks).unwrap();
    let a = Path::new(&chunks).join("part0.tmp").to_string_lossy().into_owned();
    fs::write(&a, [1]).unwrap();
    fs::write(Path::new(&chunks).join("someone-elses.dat"), [2]).unwrap();

    FileMerger::new().cleanup_chunks(std::slice::from_ref(&a), &chunks);
    assert!(Path::new(&chunks).exists());
    assert!(!Path::new(&a).exists());
}

#[test]
fn cleanup_chunks_shrugs_at_a_file_that_is_already_gone() {
    // Best-effort throughout: a missing or locked chunk is not worth an error
    // dialog on a path the user has already finished with.
    let folder = TempFolder::new();
    FileMerger::new().cleanup_chunks(&[folder.file("never-existed.tmp")], &folder.path_str());
}

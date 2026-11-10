//! Port of `DiskSpace.cs`: free space, and whether a download will fit in it.
//!
//! A download that runs out of disk does not fail where it could have been
//! stopped — it fails at the end. The chunks fill the temp volume over hours, and
//! then the merge needs room for the finished file as well, so a 40 GB download
//! on a drive with 50 GB free reached 100% and died in the merge. That is the one
//! moment a disk-full error costs the most, and the numbers to predict it were
//! known before the first byte.

/// A volume that cannot hold what a download still has to write to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceShortfall {
    pub volume: String,
    pub needed: i64,
    pub available: i64,
}

/// Free bytes on the volume holding `path`, or `None` when that cannot be told —
/// a UNC share, a drive that is not ready, a path that does not parse. `None` is
/// "unknown", and a check that cannot run lets the download go ahead rather than
/// refusing it.
pub fn available_bytes(path: &str) -> Option<i64> {
    let root = volume_of(path)?;
    crate::platform::disk::available_bytes(&root)
}

/// The volume root a path lives on (`C:\`), or `None` for anything a drive
/// letter cannot describe, UNC paths included.
///
/// `Path.GetPathRoot(Path.GetFullPath(path))`: a relative path, or one rooted
/// without a drive (`\temp`), is on the current directory's drive.
pub fn volume_of(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed.contains('\0') {
        return None;
    }
    let b = trimmed.as_bytes();
    let is_sep = |c: u8| c == b'\\' || c == b'/';
    if b.len() >= 2 && is_sep(b[0]) && is_sep(b[1]) {
        // UNC (`\\server\share`) and device paths (`\\?\`, `\\.\`).
        return None;
    }
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Some(format!("{}:\\", (b[0] as char).to_ascii_uppercase()));
    }
    // Relative, or rooted on the current drive.
    let cwd = std::env::current_dir().ok()?;
    let cwd = cwd.to_str()?;
    let cb = cwd.as_bytes();
    if cb.len() >= 2 && cb[0].is_ascii_alphabetic() && cb[1] == b':' {
        Some(format!("{}:\\", (cb[0] as char).to_ascii_uppercase()))
    } else {
        None
    }
}

/// Whether two paths are on the same drive, so a file can move between them by
/// renaming rather than copying. False whenever either side is unknown — the
/// copy is always correct, only slower.
pub fn same_volume(a: &str, b: &str) -> bool {
    match (volume_of(a), volume_of(b)) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(&right),
        _ => false,
    }
}

/// The first volume that cannot hold what is still to be written to it, or
/// `None` when everything fits (or nothing could be measured). Needs on the same
/// volume are added together: the temp folder and the destination on one drive
/// compete for the same free space.
pub fn find_shortfall<P, I, F>(needs: I, available: F) -> Option<SpaceShortfall>
where
    P: AsRef<str>,
    I: IntoIterator<Item = (P, i64)>,
    F: Fn(&str) -> Option<i64>,
{
    // (volume, first path seen on it, bytes), in the order the volumes appeared.
    let mut by_volume: Vec<(String, String, i64)> = Vec::new();
    for (path, bytes) in needs {
        let path = path.as_ref();
        if bytes <= 0 {
            continue;
        }
        let Some(volume) = volume_of(path) else { continue };
        match by_volume.iter_mut().find(|(v, _, _)| v.eq_ignore_ascii_case(&volume)) {
            Some(entry) => entry.2 = entry.2.saturating_add(bytes),
            None => by_volume.push((volume, path.to_string(), bytes)),
        }
    }

    by_volume.into_iter().find_map(|(volume, path, needed)| match available(&path) {
        Some(free) if free < needed => Some(SpaceShortfall { volume, needed, available: free }),
        _ => None,
    })
}

/// What a download still has to find room for, volume by volume, before it can
/// finish: the bytes not yet fetched into the temp folder, and the room the
/// merge needs at the destination.
///
/// `largest_chunk` is the biggest segment. A merge on the same volume appends
/// every chunk onto the first and deletes each one as it goes, so beyond the
/// chunks themselves it only ever needs room for the one being appended. On
/// another volume the whole file is written there.
pub fn still_needed(
    temp_folder: &str,
    destination_folder: &str,
    total_bytes: i64,
    downloaded_bytes: i64,
    largest_chunk: i64,
    chunk_count: i32,
) -> Vec<(String, i64)> {
    if total_bytes <= 0 {
        return Vec::new();
    }
    let for_the_merge = if same_volume(temp_folder, destination_folder) {
        if chunk_count > 1 { largest_chunk } else { 0 }
    } else {
        total_bytes
    };
    vec![
        (temp_folder.to_string(), (total_bytes - downloaded_bytes).max(0)),
        (destination_folder.to_string(), for_the_merge),
    ]
}

/// A scratch directory that deletes itself at the end of a test (the C#
/// `TempFolder`). Each gets its own, named with a fresh id, so tests can run in
/// parallel without colliding. Shared by the unit tests of this crate.
#[cfg(test)]
pub(crate) mod scratch {
    use std::path::{Path, PathBuf};

    pub struct TempFolder {
        path: PathBuf,
    }

    impl TempFolder {
        pub fn new() -> Self {
            let path = std::env::temp_dir().join("QuickByte.Tests").join(uuid::Uuid::new_v4().simple().to_string());
            std::fs::create_dir_all(&path).expect("create temp folder");
            Self { path }
        }
        pub fn path(&self) -> &Path {
            &self.path
        }
        pub fn path_str(&self) -> String {
            self.path.to_string_lossy().into_owned()
        }
        /// Full path of `name` inside this folder.
        pub fn file(&self, name: &str) -> String {
            self.path.join(name).to_string_lossy().into_owned()
        }
        /// Writes `bytes` to `name` and returns its full path.
        pub fn write_file(&self, name: &str, bytes: &[u8]) -> String {
            let path = self.file(name);
            std::fs::write(&path, bytes).expect("write temp file");
            path
        }
    }

    impl Drop for TempFolder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    //! Whether a download will fit, decided before it starts. A download that
    //! runs out of disk used to find out at 100%, in the merge — the most
    //! expensive moment there is.

    use super::*;

    const GB: i64 = 1024 * 1024 * 1024;

    #[test]
    fn paths_on_one_drive_share_a_volume_whatever_their_case() {
        assert!(same_volume(r"C:\Users\a\AppData\QuickByte\temp", r"c:\Downloads"));
        assert!(!same_volume(r"C:\temp", r"D:\Downloads"));
        assert_eq!(volume_of(r"d:\x\y").as_deref(), Some(r"D:\"));
        assert_eq!(volume_of(""), None);
    }

    #[test]
    fn a_share_is_never_assumed_to_be_the_same_volume_as_anything() {
        // Renaming across it is not possible; copying always is.
        assert!(!same_volume(r"\\server\share\temp", r"\\server\share\downloads"));
        assert_eq!(volume_of(r"\\server\share\file.bin"), None);
    }

    #[cfg(windows)]
    #[test]
    fn the_real_system_drive_reports_some_free_space() {
        let temp = std::env::temp_dir();
        assert!(available_bytes(&temp.to_string_lossy()).is_some_and(|free| free > 0));
    }

    #[test]
    fn a_download_on_one_drive_needs_the_rest_of_the_file_plus_one_chunk_for_the_merge() {
        let needs = still_needed(r"C:\temp", r"C:\Downloads", 40 * GB, 10 * GB, 5 * GB, 8);
        assert_eq!(needs, [(r"C:\temp".to_string(), 30 * GB), (r"C:\Downloads".to_string(), 5 * GB)]);
    }

    #[test]
    fn a_single_chunk_on_one_drive_is_renamed_and_needs_nothing_more() {
        let needs = still_needed(r"C:\temp", r"C:\Downloads", 40 * GB, 0, 40 * GB, 1);
        assert_eq!(needs.iter().find(|n| n.0 == r"C:\Downloads").map(|n| n.1), Some(0));
    }

    #[test]
    fn a_download_to_another_drive_needs_the_whole_file_there() {
        let needs = still_needed(r"C:\temp", r"D:\Downloads", 40 * GB, 10 * GB, 5 * GB, 8);
        assert_eq!(needs.iter().find(|n| n.0 == r"D:\Downloads").map(|n| n.1), Some(40 * GB));
    }

    #[test]
    fn a_file_of_unknown_size_asks_for_nothing() {
        assert!(still_needed(r"C:\temp", r"C:\Downloads", 0, 0, 0, 1).is_empty());
    }

    #[test]
    fn needs_on_the_same_drive_are_added_together() {
        // 30 GB of chunks and 5 GB for the merge do not each fit in 32 GB free —
        // they have to fit together.
        let shortfall = find_shortfall([(r"C:\temp", 30 * GB), (r"C:\Downloads", 5 * GB)], |_| Some(32 * GB));
        assert_eq!(shortfall, Some(SpaceShortfall { volume: r"C:\".into(), needed: 35 * GB, available: 32 * GB }));
    }

    #[test]
    fn needs_that_fit_report_no_shortfall() {
        let shortfall = find_shortfall([(r"C:\temp", 30 * GB), (r"D:\Downloads", 40 * GB)], |path| {
            Some(if path.starts_with('C') { 31 * GB } else { 41 * GB })
        });
        assert_eq!(shortfall, None);
    }

    #[test]
    fn a_drive_that_cannot_be_measured_does_not_stop_a_download() {
        assert_eq!(find_shortfall([(r"C:\temp", 30 * GB)], |_| None), None);
    }

    #[test]
    fn the_shortfall_message_names_the_drive_and_both_amounts() {
        let error = crate::CoreError::InsufficientDiskSpace(SpaceShortfall { volume: r"C:\".into(), needed: 35 * GB, available: 32 * GB });
        assert_eq!(error.to_string(), "Not enough free space on C: — 35.00 GB is needed and 32.00 GB is free.");
    }
}

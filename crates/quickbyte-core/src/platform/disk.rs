//! Free space on a volume (`DriveInfo.AvailableFreeSpace`), through
//! `GetDiskFreeSpaceExW`.

/// Free bytes available to the current user on the volume whose root is `root`
/// (`C:\`), or `None` when the volume cannot be measured — a drive that is not
/// ready, one that does not exist, an empty card reader. `None` is "unknown",
/// never "full".
#[cfg(windows)]
pub fn available_bytes(root: &str) -> Option<i64> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows::core::HSTRING;

    if root.is_empty() {
        return None;
    }
    let name = HSTRING::from(root);
    let mut free_to_caller: u64 = 0;
    // SAFETY: `name` is a live, NUL-terminated wide string for the duration of
    // the call, and the out-pointer refers to a local that outlives it.
    unsafe { GetDiskFreeSpaceExW(&name, Some(&mut free_to_caller), None, None) }.ok()?;
    Some(i64::try_from(free_to_caller).unwrap_or(i64::MAX))
}

#[cfg(not(windows))]
pub fn available_bytes(_root: &str) -> Option<i64> {
    None
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn the_system_drive_can_be_measured() {
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        assert!(available_bytes(&format!("{system}\\")).is_some_and(|free| free > 0));
    }

    #[test]
    fn a_volume_that_does_not_exist_is_unknown() {
        // Every letter from the end of the alphabet down is tried until one is not mapped.
        let missing = ('D'..='Z').rev().map(|c| format!("{c}:\\")).find(|root| !std::path::Path::new(root).exists());
        if let Some(root) = missing {
            assert_eq!(available_bytes(&root), None);
        }
        assert_eq!(available_bytes(""), None);
    }
}

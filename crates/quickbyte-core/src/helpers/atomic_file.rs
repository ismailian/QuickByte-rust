//! Crash-safe file replacement — an improvement over the C# repositories, which
//! wrote `downloads.json` in place (a crash mid-write left it truncated, and the
//! whole download list with it).
//!
//! The new content goes to `<file>.tmp`, is flushed to disk, and is then renamed
//! over the original. `std::fs::rename` is `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`
//! on Windows, so a reader — the agent reading `queues.json`, say — sees either
//! the old file or the new one, never half of one.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

/// Writes `bytes` to `path` atomically, creating the parent folder if needed.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = Path::new(&tmp_name);
    {
        let mut file = File::create(tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    // A reader holding the file open (another process mid-read) can make the
    // rename fail with a sharing violation for a moment; try a few times.
    let mut last = None;
    for attempt in 0..5 {
        match fs::rename(tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
            }
        }
    }
    let _ = fs::remove_file(tmp);
    Err(last.unwrap_or_else(|| io::Error::other("rename failed")))
}

/// Serializes `value` as indented JSON (two spaces, no BOM) and writes it atomically.
pub fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write(path, &bytes)
}

/// Reads and deserializes a JSON file, tolerating a UTF-8 BOM.
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(crate::serde_compat::strip_bom(&bytes)).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_file_and_leaves_no_temp_behind() {
        let dir = std::env::temp_dir().join(format!("qb-atomic-{}", uuid::Uuid::new_v4()));
        let path = dir.join("a.json");
        write_json(&path, &vec![1, 2, 3]).unwrap();
        write_json(&path, &vec![4]).unwrap();
        let back: Vec<i32> = read_json(&path).unwrap();
        assert_eq!(back, vec![4]);
        assert!(!dir.join("a.json.tmp").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}

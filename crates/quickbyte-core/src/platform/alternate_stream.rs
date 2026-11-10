//! NTFS alternate data streams (`path:stream`), for the Mark of the Web's
//! `Zone.Identifier`.
//!
//! Win32 opens `C:\dir\file.exe:Zone.Identifier` as the named stream of
//! `file.exe`, so the ordinary file APIs reach it; what this module adds is the
//! rule that the host file is never created just to hang a stream on.

use std::io;
use std::path::Path;

/// `file_path:stream_name`.
pub fn stream_path(file_path: &str, stream_name: &str) -> String {
    format!("{file_path}:{stream_name}")
}

/// Writes `contents` to the named stream of an existing file, replacing what the
/// stream held. Fails — without creating anything — when the file is not there,
/// and fails on a volume with no alternate streams (FAT32, exFAT, many shares).
pub fn write(file_path: &str, stream_name: &str, contents: &[u8]) -> io::Result<()> {
    // Never create the file: a stream path opened for writing on a file that is
    // not there makes an empty one to hang the stream on.
    if !Path::new(file_path).is_file() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "the file is not there"));
    }
    std::fs::write(stream_path(file_path, stream_name), contents)
}

/// Reads the named stream of a file.
pub fn read(file_path: &str, stream_name: &str) -> io::Result<Vec<u8>> {
    std::fs::read(stream_path(file_path, stream_name))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_stream_is_written_beside_the_contents_and_read_back() {
        let dir = std::env::temp_dir().join(format!("qb-ads-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.bin").to_string_lossy().into_owned();
        std::fs::write(&file, [1, 2, 3]).unwrap();

        // The temp folder is on NTFS on any normal Windows install.
        write(&file, "Test.Stream", b"hello").unwrap();
        assert_eq!(read(&file, "Test.Stream").unwrap(), b"hello");
        assert_eq!(std::fs::read(&file).unwrap(), [1, 2, 3]);

        let missing = dir.join("missing.bin").to_string_lossy().into_owned();
        assert!(write(&missing, "Test.Stream", b"x").is_err());
        assert!(!Path::new(&missing).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

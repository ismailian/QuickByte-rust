//! Port of `FileChecksum.cs`: checks a finished download against the checksum
//! its publisher lists — the only way to know that what arrived is the file that
//! was published, byte for byte, and not a truncated, corrupted or substituted one.
//!
//! It reads the forms people actually paste: a bare hex string (whose length
//! says which hash it is), a `sha256:`-style prefix, a line of `sha256sum`
//! output with the file name after it, or the BSD `SHA256 (file) = …` form.
//!
//! MD5 and SHA-1 are accepted because they are still what many download pages
//! list. They detect a damaged download perfectly well; they are no defence
//! against a file someone deliberately forged to match, and the window that
//! takes the value says so.

use std::io::Read;

use serde_repr::{Deserialize_repr, Serialize_repr};
use sha2::Digest;
use tokio_util::sync::CancellationToken;

use crate::error::{CoreError, Result};
use crate::traits::ProgressFn;

const BUFFER_SIZE: usize = 1024 * 1024;
const PROGRESS_INTERVAL_BYTES: i64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChecksumAlgorithm {
    Md5,
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

/// Persisted on `DownloadItem.ChecksumResult` as a number.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum ChecksumResult {
    /// No checksum was given, or checking was interrupted.
    #[default]
    NotChecked = 0,
    Match = 1,
    Mismatch = 2,
}

/// A checksum a publisher gave for a file: which hash, and its value in lower-case hex.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExpectedChecksum {
    pub algorithm: ChecksumAlgorithm,
    /// Lower-case hex.
    pub hex: String,
}

impl ExpectedChecksum {
    pub fn new(algorithm: ChecksumAlgorithm, hex: impl Into<String>) -> Self {
        Self { algorithm, hex: hex.into() }
    }
}

/// The display name: `MD5`, `SHA-1`, `SHA-256`, `SHA-384`, `SHA-512`.
pub fn name(algorithm: ChecksumAlgorithm) -> &'static str {
    match algorithm {
        ChecksumAlgorithm::Md5 => "MD5",
        ChecksumAlgorithm::Sha1 => "SHA-1",
        ChecksumAlgorithm::Sha256 => "SHA-256",
        ChecksumAlgorithm::Sha384 => "SHA-384",
        ChecksumAlgorithm::Sha512 => "SHA-512",
    }
}

/// Whether the hash is too weak to stand against a deliberately forged file.
pub fn is_weak(algorithm: ChecksumAlgorithm) -> bool {
    matches!(algorithm, ChecksumAlgorithm::Md5 | ChecksumAlgorithm::Sha1)
}

impl std::fmt::Display for ExpectedChecksum {
    /// `sha256:3f…` — the form persisted in `DownloadItem.ExpectedChecksum`, so
    /// the algorithm is never guessed twice.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", name(self.algorithm).replace('-', "").to_ascii_lowercase(), self.hex)
    }
}

const PREFIXES: [(&str, ChecksumAlgorithm); 9] = [
    ("sha-256", ChecksumAlgorithm::Sha256),
    ("sha256", ChecksumAlgorithm::Sha256),
    ("sha-384", ChecksumAlgorithm::Sha384),
    ("sha384", ChecksumAlgorithm::Sha384),
    ("sha-512", ChecksumAlgorithm::Sha512),
    ("sha512", ChecksumAlgorithm::Sha512),
    ("sha-1", ChecksumAlgorithm::Sha1),
    ("sha1", ChecksumAlgorithm::Sha1),
    ("md5", ChecksumAlgorithm::Md5),
];

/// The checksum in `text`, or `None` when there is none to find.
pub fn parse(text: Option<&str>) -> Option<ExpectedChecksum> {
    let text = text?;
    let mut value = text.trim();
    if value.is_empty() {
        return None;
    }

    // A prefix names the algorithm outright: "sha256:…", "SHA-256=…", "md5 …".
    let mut named = None;
    for (prefix, algorithm) in PREFIXES {
        let Some(head) = value.get(..prefix.len()) else { continue };
        if !head.eq_ignore_ascii_case(prefix) {
            continue;
        }
        let rest = &value[prefix.len()..];
        if !rest.starts_with([':', '=', ' ', '(']) {
            continue;
        }
        named = Some(algorithm);
        value = rest;
        break;
    }

    // What is left may still carry a file name (sha256sum's "hex  name", BSD's
    // "(name) = hex"): the checksum is whichever token is all hex.
    let hex = value
        .split([' ', '\t', '=', ':', '*', '(', ')'])
        .filter(|t| !t.is_empty())
        .find(|t| is_hex_hash(t))?;

    let by_length = algorithm_for_length(hex.len())?;
    if named.is_some_and(|n| n != by_length) {
        return None;
    }
    Some(ExpectedChecksum { algorithm: by_length, hex: hex.to_ascii_lowercase() })
}

/// The persisted form, `sha256:…`; `None` for anything else.
pub fn from_stored(stored: Option<&str>) -> Option<ExpectedChecksum> {
    parse(stored)
}

fn algorithm_for_length(hex_length: usize) -> Option<ChecksumAlgorithm> {
    Some(match hex_length {
        32 => ChecksumAlgorithm::Md5,
        40 => ChecksumAlgorithm::Sha1,
        64 => ChecksumAlgorithm::Sha256,
        96 => ChecksumAlgorithm::Sha384,
        128 => ChecksumAlgorithm::Sha512,
        _ => return None,
    })
}

fn is_hex_hash(token: &str) -> bool {
    algorithm_for_length(token.len()).is_some() && token.bytes().all(|b| b.is_ascii_hexdigit())
}

enum Hasher {
    Md5(md5::Md5),
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha384(sha2::Sha384),
    Sha512(sha2::Sha512),
}

impl Hasher {
    fn new(algorithm: ChecksumAlgorithm) -> Self {
        match algorithm {
            ChecksumAlgorithm::Md5 => Self::Md5(md5::Md5::new()),
            ChecksumAlgorithm::Sha1 => Self::Sha1(sha1::Sha1::new()),
            ChecksumAlgorithm::Sha256 => Self::Sha256(sha2::Sha256::new()),
            ChecksumAlgorithm::Sha384 => Self::Sha384(sha2::Sha384::new()),
            ChecksumAlgorithm::Sha512 => Self::Sha512(sha2::Sha512::new()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Md5(h) => h.update(data),
            Self::Sha1(h) => h.update(data),
            Self::Sha256(h) => h.update(data),
            Self::Sha384(h) => h.update(data),
            Self::Sha512(h) => h.update(data),
        }
    }

    fn finish_hex(self) -> String {
        match self {
            Self::Md5(h) => to_hex(&h.finalize()),
            Self::Sha1(h) => to_hex(&h.finalize()),
            Self::Sha256(h) => to_hex(&h.finalize()),
            Self::Sha384(h) => to_hex(&h.finalize()),
            Self::Sha512(h) => to_hex(&h.finalize()),
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0xF) as usize] as char);
    }
    out
}

/// Hashes a file on the calling thread, reporting bytes read every 16 MiB and
/// once at the end. Checks `cancel` between reads.
pub fn compute_blocking(
    path: &str,
    algorithm: ChecksumAlgorithm,
    progress: Option<&ProgressFn<i64>>,
    cancel: &CancellationToken,
) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Hasher::new(algorithm);
    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut read: i64 = 0;
    let mut last_reported: i64 = 0;
    loop {
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let count = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        hasher.update(&buffer[..count]);
        read += count as i64;
        if read - last_reported >= PROGRESS_INTERVAL_BYTES {
            last_reported = read;
            if let Some(report) = progress {
                report(read);
            }
        }
    }
    if let Some(report) = progress {
        report(read);
    }
    Ok(hasher.finish_hex())
}

/// Hashes a file off the async threads (hashing is CPU-bound), reporting bytes read.
pub async fn compute(
    path: &str,
    algorithm: ChecksumAlgorithm,
    progress: Option<ProgressFn<i64>>,
    cancel: &CancellationToken,
) -> Result<String> {
    let path = path.to_string();
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || compute_blocking(&path, algorithm, progress.as_ref(), &cancel))
        .await
        .map_err(|e| CoreError::other(format!("The checksum could not be computed: {e}")))?
}

/// Checks `path` against `expected`.
pub async fn verify(
    path: &str,
    expected: &ExpectedChecksum,
    progress: Option<ProgressFn<i64>>,
    cancel: &CancellationToken,
) -> Result<ChecksumResult> {
    let actual = compute(path, expected.algorithm, progress, cancel).await?;
    Ok(if actual.eq_ignore_ascii_case(&expected.hex) { ChecksumResult::Match } else { ChecksumResult::Mismatch })
}

#[cfg(test)]
mod tests {
    //! Parsing is the part with the traps — people paste whatever the download
    //! page shows — and the hashes themselves are pinned against the standard
    //! "abc" test vectors.

    use std::sync::Arc;

    use super::*;
    use crate::helpers::disk_space::scratch::TempFolder;

    const SHA256_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const MD5_ABC: &str = "900150983cd24fb0d6963f7d28e17f72";
    const SHA1_ABC: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";
    const SHA384_ABC: &str =
        "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7";
    const SHA512_ABC: &str = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";

    const VECTORS: [(ChecksumAlgorithm, &str); 5] = [
        (ChecksumAlgorithm::Md5, MD5_ABC),
        (ChecksumAlgorithm::Sha1, SHA1_ABC),
        (ChecksumAlgorithm::Sha256, SHA256_ABC),
        (ChecksumAlgorithm::Sha384, SHA384_ABC),
        (ChecksumAlgorithm::Sha512, SHA512_ABC),
    ];

    // -------------------------------------------------------------- parsing --

    #[test]
    fn a_bare_hex_value_says_which_hash_it_is_by_its_length() {
        for (expected, hex) in VECTORS {
            let parsed = parse(Some(hex)).unwrap();
            assert_eq!(parsed.algorithm, expected);
            assert_eq!(parsed.hex, hex);
        }
    }

    #[test]
    fn the_forms_download_pages_list_all_read_as_the_same_checksum() {
        let pasted = [
            format!("sha256:{SHA256_ABC}"),
            format!("SHA-256={SHA256_ABC}"),
            format!("SHA256 {SHA256_ABC}"),
            "  BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD  ".to_string(),
            format!("{SHA256_ABC}  QuickByte-1.13.0-x64.exe"),
            format!("{SHA256_ABC} *QuickByte-1.13.0-x64.exe"),
            format!("SHA256 (QuickByte-1.13.0-x64.exe) = {SHA256_ABC}"),
        ];
        for text in pasted {
            assert_eq!(parse(Some(&text)), Some(ExpectedChecksum::new(ChecksumAlgorithm::Sha256, SHA256_ABC)), "{text}");
        }
    }

    #[test]
    fn a_named_hash_of_the_wrong_length_is_refused_rather_than_guessed() {
        assert_eq!(parse(Some(&format!("md5:{SHA256_ABC}"))), None);
        assert_eq!(parse(Some(&format!("sha1:{MD5_ABC}"))), None);
    }

    #[test]
    fn anything_that_is_not_a_checksum_is_none() {
        for text in [
            Some(""),
            None,
            Some("not a checksum"),
            Some("zz7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015"),
            Some("sha"),
            Some("é"),
        ] {
            assert_eq!(parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_stored_form_names_its_hash_and_reads_back_exactly() {
        let checksum = ExpectedChecksum::new(ChecksumAlgorithm::Sha512, SHA512_ABC);
        assert_eq!(checksum.to_string(), format!("sha512:{SHA512_ABC}"));
        assert_eq!(from_stored(Some(&checksum.to_string())), Some(checksum));
        assert_eq!(ExpectedChecksum::new(ChecksumAlgorithm::Sha1, SHA1_ABC).to_string(), format!("sha1:{SHA1_ABC}"));
    }

    #[test]
    fn only_md5_and_sha1_are_called_weak() {
        assert!(is_weak(ChecksumAlgorithm::Md5));
        assert!(is_weak(ChecksumAlgorithm::Sha1));
        assert!(!is_weak(ChecksumAlgorithm::Sha256));
        assert!(!is_weak(ChecksumAlgorithm::Sha384));
        assert!(!is_weak(ChecksumAlgorithm::Sha512));
    }

    // ------------------------------------------------------------ computing --

    #[tokio::test]
    async fn every_hash_matches_its_standard_test_vector() {
        let folder = TempFolder::new();
        let file = folder.write_file("abc.txt", b"abc");
        for (algorithm, expected) in VECTORS {
            assert_eq!(compute(&file, algorithm, None, &CancellationToken::new()).await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn a_file_larger_than_one_read_hashes_the_same_as_in_one_piece() {
        let folder = TempFolder::new();
        let content: Vec<u8> = (0..3 * 1024 * 1024 + 17).map(|i: u32| i.wrapping_mul(31) as u8).collect();
        let file = folder.write_file("big.bin", &content);
        let expected = to_hex(&sha2::Sha256::digest(&content));
        assert_eq!(compute(&file, ChecksumAlgorithm::Sha256, None, &CancellationToken::new()).await.unwrap(), expected);
    }

    #[tokio::test]
    async fn verifying_says_match_or_mismatch() {
        let folder = TempFolder::new();
        let file = folder.write_file("abc.txt", b"abc");
        let none = CancellationToken::new();
        assert_eq!(
            verify(&file, &ExpectedChecksum::new(ChecksumAlgorithm::Md5, MD5_ABC), None, &none).await.unwrap(),
            ChecksumResult::Match
        );
        assert_eq!(
            verify(&file, &ExpectedChecksum::new(ChecksumAlgorithm::Md5, "0".repeat(32)), None, &none).await.unwrap(),
            ChecksumResult::Mismatch
        );
    }

    #[tokio::test]
    async fn progress_ends_on_the_file_size() {
        let folder = TempFolder::new();
        let file = folder.write_file("abc.txt", b"abc");
        let reports = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink = reports.clone();
        let progress: ProgressFn<i64> = Arc::new(move |n| sink.lock().push(n));
        compute(&file, ChecksumAlgorithm::Sha256, Some(progress), &CancellationToken::new()).await.unwrap();
        assert_eq!(reports.lock().last().copied(), Some(3));
    }

    #[tokio::test]
    async fn a_cancelled_check_stops_and_a_missing_file_is_an_error() {
        let folder = TempFolder::new();
        let file = folder.write_file("abc.txt", b"abc");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(compute(&file, ChecksumAlgorithm::Md5, None, &cancel).await, Err(CoreError::Cancelled)));
        assert!(compute(&folder.file("missing"), ChecksumAlgorithm::Md5, None, &CancellationToken::new()).await.is_err());
    }
}

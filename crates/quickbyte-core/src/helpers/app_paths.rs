//! Where QuickByte keeps its state. Port of `AppPaths.cs`.
//!
//! `%APPDATA%\QuickByte` — the same folder the C# build used, which is what lets
//! an upgraded install carry on with the user's downloads, queues and settings.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const TEMP_FOLDER_NAME: &str = "temp";

/// `%APPDATA%\QuickByte` (`Environment.SpecialFolder.ApplicationData`), or the
/// sandbox folder when [`sandbox`] is set.
pub fn data() -> &'static Path {
    static DATA: OnceLock<PathBuf> = OnceLock::new();
    DATA.get_or_init(|| sandbox().map(Path::to_path_buf).unwrap_or_else(|| roaming_app_data().join("QuickByte")))
}

/// The environment variable that runs a build in a developer sandbox.
pub const SANDBOX_VARIABLE: &str = "QUICKBYTE_SANDBOX";

/// A developer sandbox: `QUICKBYTE_SANDBOX=<folder>` (not part of the C# build).
///
/// A dev build started on a machine that has QuickByte installed would
/// otherwise share the user's profile, hand its launch to the installed copy
/// (same mutex), and rewrite or delete the user's `Run` values. In the sandbox
/// the data folder is `<folder>`, the mutex and pipe names carry a `.sandbox`
/// suffix, and nothing is written to the registry or Task Scheduler.
pub fn sandbox() -> Option<&'static Path> {
    static SANDBOX: OnceLock<Option<PathBuf>> = OnceLock::new();
    SANDBOX
        .get_or_init(|| std::env::var_os(SANDBOX_VARIABLE).filter(|v| !v.is_empty()).map(PathBuf::from))
        .as_deref()
}

/// `""`, or `".sandbox"` in a sandbox — appended to every machine-wide name
/// (mutexes, pipes) so a sandboxed build never meets the installed one.
pub fn instance_suffix() -> &'static str {
    if sandbox().is_some() { ".sandbox" } else { "" }
}

/// The chunk folder: `%APPDATA%\QuickByte\temp`. Not `%TEMP%`, which gets swept.
pub fn temp() -> PathBuf {
    temp_in(data())
}

pub fn temp_in(data_folder: &Path) -> PathBuf {
    data_folder.join(TEMP_FOLDER_NAME)
}

fn roaming_app_data() -> PathBuf {
    if let Some(p) = std::env::var_os("APPDATA").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile).join("AppData").join("Roaming");
    }
    PathBuf::from(".")
}

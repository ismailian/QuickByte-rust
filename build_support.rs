// Shared by every binary's build.rs (`include!`d): stamps the Win32 VERSIONINFO
// resource and the icon, so Explorer's Properties page and Task Manager's
// Startup tab show the same names the C# executables showed.
//
// QB_VERSION_SUFFIX (e.g. "beta.1") and QB_BUILD_REVISION (e.g. "137") let a
// build server label a build without editing Cargo.toml — the replacement for
// `-p:VersionSuffix` / `-p:BuildRevision`.

#[allow(dead_code)]
fn stamp_windows_resources(file_description: &str, original_filename: &str) {
    println!("cargo:rerun-if-env-changed=QB_VERSION_SUFFIX");
    println!("cargo:rerun-if-env-changed=QB_BUILD_REVISION");
    let icon = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/quickbyte.ico");
    println!("cargo:rerun-if-changed={icon}");

    let version = env!("CARGO_PKG_VERSION");
    let mut parts = version.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let (major, minor, patch) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    let revision: u64 = std::env::var("QB_BUILD_REVISION").ok().and_then(|r| r.parse().ok()).unwrap_or(0);
    let product_version = match std::env::var("QB_VERSION_SUFFIX") {
        Ok(suffix) if !suffix.trim().is_empty() => format!("{version}-{}", suffix.trim()),
        _ => version.to_string(),
    };
    // Read back at runtime by quickbyte_shell::app_version (the AssemblyInformationalVersion equivalent).
    println!("cargo:rustc-env=QB_PRODUCT_VERSION={product_version}");
    println!("cargo:rustc-env=QB_FILE_VERSION={major}.{minor}.{patch}.{revision}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let packed = (major << 48) | (minor << 32) | (patch << 16) | revision;
    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon)
        .set("FileDescription", file_description)
        .set("ProductName", "QuickByte")
        .set("CompanyName", "QuickByte")
        .set("LegalCopyright", "Copyright \u{a9} 2026 QuickByte")
        .set("OriginalFilename", original_filename)
        .set("InternalName", original_filename.trim_end_matches(".exe"))
        .set("FileVersion", &format!("{major}.{minor}.{patch}.{revision}"))
        .set("ProductVersion", &product_version)
        .set_version_info(winresource::VersionInfo::FILEVERSION, packed)
        .set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed)
        .set_language(0x0409);
    if let Err(e) = res.compile() {
        println!("cargo:warning=could not embed Windows resources: {e}");
    }
}

//! MIGRATION.md §5: a profile written by the C# build must load here and save
//! back to the same JSON. Runs against the fixtures in `tests/fixtures`, and —
//! read-only — against this machine's real `%APPDATA%\QuickByte` when present.

use std::path::{Path, PathBuf};

use quickbyte_core::models::{DownloadItem, DownloadQueue, DownloadSettings};
use quickbyte_core::serde_compat::strip_bom;
use serde_json::Value;

/// Re-encrypting a password yields a different ciphertext every time, so a
/// protected value is compared by whether it is present, not by its bytes.
fn normalise(mut v: Value) -> Value {
    match &mut v {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if key == "ProtectedPassword" {
                    *value = Value::Bool(!value.is_null());
                } else {
                    *value = normalise(std::mem::take(value));
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                *item = normalise(std::mem::take(item));
            }
        }
        // 8832585.334748305 and 8832585.334748306 are the same double to .NET's
        // shortest round-trip printer and to serde_json's; compare as f64.
        Value::Number(n) if n.is_f64() => {
            let f = n.as_f64().unwrap();
            return serde_json::json!(f);
        }
        _ => {}
    }
    v
}

fn diff(path: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for k in x.keys().chain(y.keys()) {
                if let (Some(p), Some(q)) = (x.get(k), y.get(k)) {
                    diff(&format!("{path}.{k}"), p, q, out);
                } else if x.get(k) != y.get(k) {
                    out.push(format!("{path}.{k}: {:?} vs {:?}", x.get(k), y.get(k)));
                }
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                diff(&format!("{path}[{i}]"), p, q, out);
            }
        }
        _ if a != b => out.push(format!("{path}: {a} vs {b}")),
        _ => {}
    }
}

fn assert_same(written: Value, original: Value, what: &Path) {
    let (w, o) = (normalise(written), normalise(original));
    let mut out = Vec::new();
    diff("$", &w, &o, &mut out);
    out.dedup();
    assert!(out.is_empty(), "{} did not round-trip (written vs original):
{}", what.display(), out.join("
"));
}

fn assert_round_trips<T>(path: &Path)
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let bytes = std::fs::read(path).unwrap();
    let original: Value = serde_json::from_slice(strip_bom(&bytes)).unwrap();
    let typed: T = serde_json::from_slice(strip_bom(&bytes)).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let written = serde_json::to_value(&typed).unwrap();
    assert_same(written, original, path);
}

fn profiles() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures")];
    if let Some(app_data) = std::env::var_os("APPDATA") {
        let live = PathBuf::from(app_data).join("QuickByte");
        if live.is_dir() {
            dirs.push(live);
        }
    }
    dirs
}

#[test]
fn settings_round_trip() {
    for dir in profiles() {
        let path = dir.join("settings.json");
        if path.exists() {
            assert_round_trips::<DownloadSettings>(&path);
        }
    }
}

#[test]
fn queues_round_trip() {
    for dir in profiles() {
        let path = dir.join("queues.json");
        if path.exists() {
            assert_round_trips::<Vec<DownloadQueue>>(&path);
        }
    }
}

#[test]
fn downloads_round_trip() {
    for dir in profiles() {
        let path = dir.join("downloads.json");
        if !path.exists() {
            continue;
        }
        // Computed fields are recomputed on save; recompute them on the parsed
        // items the way the repository does, then compare.
        let bytes = std::fs::read(&path).unwrap();
        let original: Value = serde_json::from_slice(strip_bom(&bytes)).unwrap();
        let mut items: Vec<DownloadItem> = serde_json::from_slice(strip_bom(&bytes)).unwrap();
        for item in &mut items {
            item.refresh_derived();
        }
        let written = serde_json::to_value(&items).unwrap();
        assert_same(written, original, &path);
    }
}

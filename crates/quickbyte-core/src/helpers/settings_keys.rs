//! Port of `SettingsKeys.cs`: [`DownloadSettings`] as a flat set of named values
//! — what the settings service's `get` / `save_value` / `save_many` are made of,
//! kept pure here so the conversion rules can be tested without a settings file.
//!
//! A key is a property name of `DownloadSettings`, matched without regard to
//! case: the same name settings.json already uses, so there is one vocabulary for
//! the file and for code. Only what is persisted has a key — `temp_folder` is
//! derived, not set.
//!
//! Values are strings in a form a person could type: numbers in the invariant
//! culture, `true`/`false`, an enum by its name, and a blank for "not set" on a
//! value that may be unset. The two structured settings (`Proxy` and
//! `CategoryFolders`) are their JSON, exactly as the file holds them — so the
//! proxy password is only ever its encrypted form here.
//!
//! The C# walked the properties by reflection; here the table of keys is written
//! out, and values travel through the same `serde_json::Value` the file is made
//! of, so a key's name and its wire name can never disagree.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::enums::{AppTheme, VirusTotalAction};
use crate::error::CoreError;
use crate::helpers::file_category::FileCategory;
use crate::models::{DownloadSettings, ProxySettings};

/// What a setting holds, which decides how its text is read and written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Int,
    Long,
    Bool,
    Text,
    /// A string that may be unset: a blank reads as `null`.
    OptionalText,
    Theme,
    VirusTotalAction,
    Proxy,
    CategoryFolders,
}

/// Every key, in the order `DownloadSettings` declares them.
const TABLE: [(&str, Kind); 31] = [
    ("DefaultConnectionsCount", Kind::Int),
    ("DynamicSegmentation", Kind::Bool),
    ("MaxRetries", Kind::Int),
    ("RetryDelayMilliseconds", Kind::Int),
    ("CustomUserAgent", Kind::OptionalText),
    ("DefaultDownloadFolder", Kind::Text),
    ("SortIntoCategoryFolders", Kind::Bool),
    ("Proxy", Kind::Proxy),
    ("CategoryFolders", Kind::CategoryFolders),
    ("ProgressUpdateIntervalMilliseconds", Kind::Int),
    ("StreamBufferSizeBytes", Kind::Int),
    ("MaxConcurrentDownloads", Kind::Int),
    ("GlobalSpeedLimitBytesPerSecond", Kind::Long),
    ("BrowserIntegrationEnabled", Kind::Bool),
    ("BrowserIntegrationPort", Kind::Int),
    ("BrowserIntegrationToken", Kind::Text),
    ("StartWithWindows", Kind::Bool),
    ("StartMinimized", Kind::Bool),
    ("KeepAwakeWhileDownloading", Kind::Bool),
    ("AutoOpenDetailsWindow", Kind::Bool),
    ("ShowCompletionWindow", Kind::Bool),
    ("Appearance", Kind::Theme),
    ("MonitorClipboard", Kind::Bool),
    ("ClipboardFileTypes", Kind::Text),
    ("VirusTotalEnabled", Kind::Bool),
    // Its encrypted form, as the file holds it — like the proxy password. A plain
    // key written here reads back as itself (see `models::protected_text`).
    ("ProtectedVirusTotalApiKey", Kind::Text),
    ("VirusTotalMaliciousThreshold", Kind::Int),
    ("VirusTotalUploadPrivateFiles", Kind::Bool),
    ("VirusTotalMaxUploadMegabytes", Kind::Int),
    ("VirusTotalMaliciousAction", Kind::VirusTotalAction),
    ("VirusTotalSuspiciousAction", Kind::VirusTotalAction),
];

const THEME_NAMES: [(&str, AppTheme); 3] = [("System", AppTheme::System), ("Light", AppTheme::Light), ("Dark", AppTheme::Dark)];

/// Why a by-name read or write was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettingsKeyError {
    /// `KeyNotFoundException`: there is no setting by that name.
    #[error("There is no setting called \"{0}\".")]
    UnknownKey(String),
    /// `FormatException`: the value is not one its setting can hold.
    #[error("{0}")]
    InvalidValue(String),
}

impl From<SettingsKeyError> for CoreError {
    fn from(e: SettingsKeyError) -> Self {
        CoreError::Invalid(e.to_string())
    }
}

/// Every setting by key, in declaration order; lookups ignore case.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingValues(pub Vec<(&'static str, String)>);

impl SettingValues {
    pub fn get(&self, key: &str) -> Option<&str> {
        let key = key.trim();
        self.0.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.0.iter().map(|(k, v)| (*k, v.as_str()))
    }
}

/// Every key, in the order `DownloadSettings` declares them.
pub fn all() -> impl ExactSizeIterator<Item = &'static str> {
    TABLE.iter().map(|(name, _)| *name)
}

/// Every setting of `settings`, by key.
pub fn read_all(settings: &DownloadSettings) -> SettingValues {
    let object = to_object(settings);
    SettingValues(TABLE.iter().map(|&(name, kind)| (name, format(kind, object.get(name)))).collect())
}

/// One setting's value.
pub fn read(settings: &DownloadSettings, key: &str) -> Result<String, SettingsKeyError> {
    let (name, kind) = find(key)?;
    Ok(format(kind, to_object(settings).get(name)))
}

/// A copy of `settings` with `changes` applied. All or nothing: every value is
/// converted before anything is returned, so a bad one in the middle of a batch
/// leaves no half-applied copy to save. `settings` itself is never touched — a
/// running download holds the instance it started with and is promised it will
/// not change.
pub fn with<K: AsRef<str>, V: AsRef<str>>(
    settings: &DownloadSettings,
    changes: &[(K, V)],
) -> Result<DownloadSettings, SettingsKeyError> {
    let parsed = changes
        .iter()
        .map(|(key, value)| {
            let (name, kind) = find(key.as_ref())?;
            Ok((name, parse(name, kind, value.as_ref())?))
        })
        .collect::<Result<Vec<_>, SettingsKeyError>>()?;

    let mut object = to_object(settings);
    for (name, value) in parsed {
        object.insert(name.to_string(), value);
    }
    let mut copy: DownloadSettings = serde_json::from_value(Value::Object(object))
        .map_err(|e| SettingsKeyError::InvalidValue(format!("The settings could not be applied: {e}")))?;
    copy.temp_folder = settings.temp_folder.clone();
    Ok(copy)
}

/// A deep copy. (The C# went through JSON for completeness; a Rust `clone` is
/// already complete, `extra` and the derived chunk folder included.)
pub fn copy(settings: &DownloadSettings) -> DownloadSettings {
    settings.clone()
}

fn to_object(settings: &DownloadSettings) -> serde_json::Map<String, Value> {
    match serde_json::to_value(settings) {
        Ok(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    }
}

fn find(key: &str) -> Result<(&'static str, Kind), SettingsKeyError> {
    let trimmed = key.trim();
    TABLE
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(trimmed))
        .copied()
        .ok_or_else(|| SettingsKeyError::UnknownKey(key.to_string()))
}

fn format(kind: Kind, value: Option<&Value>) -> String {
    match (kind, value) {
        (_, None | Some(Value::Null)) => String::new(),
        (_, Some(Value::String(text))) => text.clone(),
        (_, Some(Value::Bool(flag))) => if *flag { "true" } else { "false" }.to_string(),
        (Kind::Theme, Some(Value::Number(n))) => {
            let number = n.as_i64().unwrap_or(-1);
            THEME_NAMES
                .iter()
                .find(|(_, theme)| *theme as i64 == number)
                .map_or_else(|| number.to_string(), |(name, _)| name.to_string())
        }
        (Kind::VirusTotalAction, Some(Value::Number(n))) => {
            let number = n.as_i64().unwrap_or(-1);
            VirusTotalAction::ALL
                .iter()
                .find(|a| **a as i64 == number)
                .map_or_else(|| number.to_string(), |a| a.name().to_string())
        }
        (_, Some(Value::Number(n))) => n.to_string(),
        (_, Some(other)) => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn parse(name: &str, kind: Kind, value: &str) -> Result<Value, SettingsKeyError> {
    let text = value.trim();
    let invalid = |expected: &str| SettingsKeyError::InvalidValue(format!("\"{value}\" is not a value {name} can hold; it takes {expected}."));

    if text.is_empty() && kind == Kind::OptionalText {
        return Ok(Value::Null);
    }

    match kind {
        // As given, not trimmed: a folder or a User-Agent is the user's to word.
        Kind::Text | Kind::OptionalText => Ok(Value::String(value.to_string())),
        Kind::Bool => {
            if text.eq_ignore_ascii_case("true") {
                Ok(Value::Bool(true))
            } else if text.eq_ignore_ascii_case("false") {
                Ok(Value::Bool(false))
            } else {
                Err(invalid("true or false"))
            }
        }
        // Names only: a number would be a value the enum may not have.
        Kind::Theme => THEME_NAMES
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(text))
            .map(|(_, theme)| Value::from(*theme as i32))
            .ok_or_else(|| invalid("System, Light, Dark")),
        Kind::VirusTotalAction => VirusTotalAction::ALL
            .iter()
            .find(|a| a.name().eq_ignore_ascii_case(text))
            .map(|a| Value::from(*a as i32))
            .ok_or_else(|| invalid("Ask, Delete, Warn")),
        Kind::Int => text.parse::<i32>().map(Value::from).map_err(|_| invalid("a whole number")),
        Kind::Long => text.parse::<i64>().map(Value::from).map_err(|_| invalid("a whole number")),
        Kind::Proxy => parse_json::<ProxySettings>(text).ok_or_else(|| invalid("a JSON value")),
        Kind::CategoryFolders => parse_json::<BTreeMap<FileCategory, String>>(text).ok_or_else(|| invalid("a JSON value")),
    }
}

/// The JSON text, checked by reading it as the setting's own type (`null` is refused).
fn parse_json<T: serde::de::DeserializeOwned>(text: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(text).ok()?;
    if value.is_null() {
        return None;
    }
    serde_json::from_value::<T>(value.clone()).ok()?;
    Some(value)
}

#[cfg(test)]
mod tests {
    //! What matters most is the thing the by-name route exists for: changing one
    //! setting leaves every other one exactly as it was, and a value that cannot
    //! be held changes nothing at all.

    use super::*;

    #[test]
    fn every_persisted_setting_has_a_key_and_the_derived_chunk_folder_does_not() {
        let keys: Vec<&str> = all().collect();
        assert!(keys.contains(&"MaxRetries"));
        assert!(keys.contains(&"Proxy"));
        assert!(!keys.contains(&"TempFolder"));
        assert_eq!(read_all(&DownloadSettings::default()).len(), keys.len());
    }

    #[test]
    fn the_table_names_every_persisted_field_and_nothing_else() {
        // A field added to DownloadSettings without a key here would be
        // unreachable by name; this catches it.
        let mut written: Vec<String> = to_object(&DownloadSettings::default()).keys().cloned().collect();
        written.sort();
        let mut keys: Vec<String> = all().map(str::to_string).collect();
        keys.sort();
        assert_eq!(written, keys);
    }

    #[test]
    fn values_read_back_in_a_form_a_person_could_type() {
        let settings = DownloadSettings {
            max_retries: 5,
            global_speed_limit_bytes_per_second: 1_048_576,
            dynamic_segmentation: false,
            appearance: AppTheme::Dark,
            custom_user_agent: None,
            ..Default::default()
        };
        assert_eq!(read(&settings, "MaxRetries").unwrap(), "5");
        assert_eq!(read(&settings, "GlobalSpeedLimitBytesPerSecond").unwrap(), "1048576");
        assert_eq!(read(&settings, "DynamicSegmentation").unwrap(), "false");
        assert_eq!(read(&settings, "Appearance").unwrap(), "Dark");
        assert_eq!(read(&settings, "CustomUserAgent").unwrap(), "");
    }

    #[test]
    fn keys_are_matched_without_regard_to_case() {
        let settings = DownloadSettings { max_retries: 7, ..Default::default() };
        assert_eq!(read(&settings, "maxretries").unwrap(), "7");
        assert_eq!(with(&settings, &[("MAXRETRIES", "2")]).unwrap().max_retries, 2);
        assert_eq!(read_all(&settings).get("maxRETRIES"), Some("7"));
    }

    #[test]
    fn a_change_returns_a_copy_and_leaves_the_original_and_every_other_setting_alone() {
        // A running download holds the instance it started with.
        let original = DownloadSettings {
            max_retries: 3,
            stream_buffer_size_bytes: 12345,
            browser_integration_token: "secret".into(),
            ..Default::default()
        };
        let changed = with(&original, &[("MaxRetries", "9")]).unwrap();
        assert_eq!(original.max_retries, 3);
        assert_eq!(changed.max_retries, 9);
        assert_eq!(changed.stream_buffer_size_bytes, 12345);
        assert_eq!(changed.browser_integration_token, "secret");
        assert_eq!(changed.temp_folder, original.temp_folder);
    }

    #[test]
    fn every_kind_of_value_can_be_written() {
        let changed = with(
            &DownloadSettings::default(),
            &[
                ("DefaultConnectionsCount", "16"),
                ("GlobalSpeedLimitBytesPerSecond", "2048"),
                ("MonitorClipboard", "True"),
                ("Appearance", "light"),
                ("CustomUserAgent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)"),
                ("CategoryFolders", r#"{"Video":"D:\\Videos"}"#),
                ("Proxy", r#"{"Mode":2,"Host":"proxy.local","Port":8080}"#),
            ],
        )
        .unwrap();
        assert_eq!(changed.default_connections_count, 16);
        assert_eq!(changed.global_speed_limit_bytes_per_second, 2048);
        assert!(changed.monitor_clipboard);
        assert_eq!(changed.appearance, AppTheme::Light);
        assert_eq!(changed.custom_user_agent.as_deref(), Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64)"));
        assert_eq!(changed.category_folders[&FileCategory::Video], r"D:\Videos");
        assert_eq!(changed.proxy.host, "proxy.local");
        assert_eq!(changed.proxy.port, 8080);
    }

    #[test]
    fn a_blank_unsets_a_setting_that_may_be_unset() {
        let settings = DownloadSettings { custom_user_agent: Some("Something/1.0".into()), ..Default::default() };
        assert_eq!(with(&settings, &[("CustomUserAgent", "  ")]).unwrap().custom_user_agent, None);
    }

    #[test]
    fn a_structured_setting_round_trips_through_its_own_value() {
        let settings = DownloadSettings {
            category_folders: BTreeMap::from([(FileCategory::Music, r"E:\Music".to_string())]),
            ..Default::default()
        };
        let json = read(&settings, "CategoryFolders").unwrap();
        let copy = with(&DownloadSettings::default(), &[("CategoryFolders", json.as_str())]).unwrap();
        assert!(json.contains("Music")); // by name, as settings.json holds it
        assert_eq!(copy.category_folders[&FileCategory::Music], r"E:\Music");
    }

    #[test]
    fn a_value_the_setting_cannot_hold_is_refused() {
        for (key, value) in [
            ("MaxRetries", "three"),
            ("MaxRetries", "1.5"),
            ("DynamicSegmentation", "yes"),
            ("Appearance", "Purple"),
            ("Appearance", "7"), // a number the enum does not have
            ("Appearance", "1"), // nor even one it does: names only
            ("Proxy", "{not json"),
            ("Proxy", ""),
            ("Proxy", "null"),
        ] {
            match with(&DownloadSettings::default(), &[(key, value)]) {
                Err(SettingsKeyError::InvalidValue(message)) => assert!(message.contains(key), "{message}"),
                other => panic!("{key}={value}: {other:?}"),
            }
        }
    }

    #[test]
    fn an_unknown_key_is_refused() {
        assert!(matches!(read(&DownloadSettings::default(), "NoSuchSetting"), Err(SettingsKeyError::UnknownKey(_))));
        assert!(matches!(read(&DownloadSettings::default(), "TempFolder"), Err(SettingsKeyError::UnknownKey(_))));
        assert!(matches!(
            with(&DownloadSettings::default(), &[("NoSuchSetting", "1")]),
            Err(SettingsKeyError::UnknownKey(_))
        ));
        assert_eq!(
            SettingsKeyError::UnknownKey("Nope".into()).to_string(),
            "There is no setting called \"Nope\"."
        );
    }

    #[test]
    fn a_bad_value_in_a_batch_changes_nothing() {
        let original = DownloadSettings::default();
        assert!(with(&original, &[("MaxRetries", "1"), ("DynamicSegmentation", "maybe")]).is_err());
        assert_eq!(original.max_retries, 3);
    }

    #[test]
    fn a_copy_is_deep() {
        let original = DownloadSettings {
            category_folders: BTreeMap::from([(FileCategory::Video, r"D:\Videos".to_string())]),
            proxy: ProxySettings { host: "proxy.local".into(), ..Default::default() },
            ..Default::default()
        };
        let mut copied = copy(&original);
        copied.category_folders.insert(FileCategory::Video, r"X:\Elsewhere".into());
        copied.proxy.host = "other.local".into();
        assert_eq!(original.category_folders[&FileCategory::Video], r"D:\Videos");
        assert_eq!(original.proxy.host, "proxy.local");
        assert_eq!(original.temp_folder, copied.temp_folder);
    }

    #[test]
    fn unknown_keys_from_a_newer_build_survive_a_change_by_name() {
        let mut settings = DownloadSettings::default();
        settings.extra.insert("SomeFutureSetting".into(), Value::String("k".into()));
        let changed = with(&settings, &[("MaxRetries", "4")]).unwrap();
        assert_eq!(changed.extra.get("SomeFutureSetting"), Some(&Value::String("k".into())));
    }

    #[test]
    fn virus_total_actions_are_read_and_written_by_name() {
        let changed = with(
            &DownloadSettings::default(),
            &[("VirusTotalMaliciousAction", "delete"), ("VirusTotalSuspiciousAction", "Warn")],
        )
        .unwrap();
        assert_eq!(changed.virus_total_malicious_action, VirusTotalAction::Delete);
        assert_eq!(changed.virus_total_suspicious_action, VirusTotalAction::Warn);
        assert_eq!(read(&changed, "VirusTotalMaliciousAction").unwrap(), "Delete");
        assert!(matches!(
            with(&DownloadSettings::default(), &[("VirusTotalMaliciousAction", "1")]),
            Err(SettingsKeyError::InvalidValue(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn a_virus_total_key_typed_by_name_is_kept_and_survives_other_changes() {
        // Typed in the clear through `qb config`; read back as the key, and still
        // the key after a change to something else re-reads the encrypted copy.
        let key = "a".repeat(64);
        let changed = with(&DownloadSettings::default(), &[("ProtectedVirusTotalApiKey", key.as_str())]).unwrap();
        assert_eq!(changed.virus_total_api_key, key);
        assert_ne!(read(&changed, "ProtectedVirusTotalApiKey").unwrap(), key, "only the encrypted form is shown");
        assert_eq!(with(&changed, &[("MaxRetries", "4")]).unwrap().virus_total_api_key, key);
    }
}

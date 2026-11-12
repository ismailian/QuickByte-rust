//! The C# `Models/` folder.
//!
//! Persisted models follow the rules in MIGRATION.md §5: PascalCase names,
//! enums as numbers, C# property initialisers as `Default`, `[JsonIgnore]` as
//! `#[serde(skip)]`, and an `extra` map that carries keys this build does not
//! know through a load/save round trip untouched.

mod download_item;
mod download_queue;
mod download_settings;
mod misc;
mod proxy_settings;
mod virus_total;

pub use download_item::*;
pub use download_queue::*;
pub use download_settings::*;
pub use misc::*;
pub use proxy_settings::*;
pub use virus_total::*;

/// Keys a persisted object carried that this build does not model — read,
/// kept, and written back as they were (MIGRATION.md §5.6).
pub type Extra = serde_json::Map<String, serde_json::Value>;

/// Reads a [`PowerAction`](crate::enums::PowerAction) number the way the C#
/// build did: System.Text.Json takes any integer into an enum, and
/// `CompletionPlan.Defined` then treats one that names no action as none. A
/// hand-edited `42` must not make the whole `downloads.json` unreadable here.
pub(crate) fn lenient_power_action<'de, D>(deserializer: D) -> Result<crate::enums::PowerAction, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use crate::enums::PowerAction;
    use serde::Deserialize;
    let number = i64::deserialize(deserializer)?;
    Ok(PowerAction::ALL.into_iter().find(|a| *a as i64 == number).unwrap_or(PowerAction::None))
}

/// Reads a [`VirusTotalAction`](crate::enums::VirusTotalAction) number the way
/// [`lenient_power_action`] reads its own: a value no action has is `Ask`, the
/// careful choice, rather than a settings file that will not load.
pub(crate) fn lenient_virus_total_action<'de, D>(deserializer: D) -> Result<crate::enums::VirusTotalAction, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use crate::enums::VirusTotalAction;
    use serde::Deserialize;
    let number = Option::<i64>::deserialize(deserializer)?.unwrap_or(0);
    Ok(VirusTotalAction::ALL.into_iter().find(|a| *a as i64 == number).unwrap_or_default())
}

/// A secret kept in a JSON file: written DPAPI-encrypted (`secret_protector`),
/// read back from that form — or, failing that, as the plain text a person
/// could have typed through `qb config` or a build that stored it unencrypted,
/// when `accept_plain` says the text looks like the secret itself.
pub(crate) mod protected_text {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::helpers::secret_protector;

    /// `null` when there is no secret, as the C# build wrote it.
    pub fn serialize<S: Serializer>(value: &str, serializer: S) -> Result<S::Ok, S::Error> {
        match secret_protector::protect(value) {
            Some(protected) => serializer.serialize_some(&protected),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        let stored = Option::<String>::deserialize(deserializer)?.unwrap_or_default();
        Ok(read(&stored, crate::helpers::virus_total::looks_like_api_key))
    }

    /// The secret in `stored`: its decrypted form, or the text itself if it
    /// does not decrypt and `accept_plain` takes it.
    pub fn read(stored: &str, accept_plain: impl Fn(&str) -> bool) -> String {
        let stored = stored.trim();
        if stored.is_empty() {
            return String::new();
        }
        let decrypted = secret_protector::unprotect(Some(stored));
        if !decrypted.is_empty() {
            return decrypted;
        }
        if accept_plain(stored) { stored.to_string() } else { String::new() }
    }
}

/// Deserializes `null` as the type's default, the way the C# setters coalesced
/// `value ?? new …()` for collections and nested objects.
pub(crate) fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::Deserialize<'de>,
{
    use serde::Deserialize;
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

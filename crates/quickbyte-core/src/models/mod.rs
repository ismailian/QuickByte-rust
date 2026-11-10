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

pub use download_item::*;
pub use download_queue::*;
pub use download_settings::*;
pub use misc::*;
pub use proxy_settings::*;

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

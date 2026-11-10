//! `ProxySettings` — Options > Proxy, persisted inside `settings.json`.

use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

use super::{Extra, null_as_default};
use crate::helpers::secret_protector;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum ProxyMode {
    /// Windows' own proxy settings — the default, and what every earlier version did.
    #[default]
    System = 0,
    /// Connect directly.
    None = 1,
    /// The proxy named in Options.
    Manual = 2,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize_repr, Deserialize_repr)]
#[repr(i32)]
pub enum ProxyKind {
    #[default]
    Http = 0,
    Socks5 = 1,
    Socks4 = 2,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ProxyWire", into = "ProxyWire")]
pub struct ProxySettings {
    pub mode: ProxyMode,
    pub kind: ProxyKind,
    pub host: String,
    pub port: i32,
    pub user_name: String,
    /// In memory only; persisted DPAPI-encrypted as `ProtectedPassword`.
    pub password: String,
    /// `example.com; *.lan; .corp.example.com; <local>` — the forms Windows takes.
    pub bypass: String,
    pub extra: Extra,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            mode: ProxyMode::System,
            kind: ProxyKind::Http,
            host: String::new(),
            port: 8080,
            user_name: String::new(),
            password: String::new(),
            bypass: String::new(),
            extra: Extra::new(),
        }
    }
}

impl ProxySettings {
    /// A host and a port that a connection could actually be made to.
    pub fn is_complete(&self) -> bool {
        Self::is_valid_host(&self.host) && (1..=65535).contains(&self.port)
    }

    /// An IP literal (optionally bracketed) or a DNS name.
    pub fn is_valid_host(host: &str) -> bool {
        if host.trim().is_empty() || host.len() > 253 {
            return false;
        }
        let trimmed = host.trim_matches(|c| c == '[' || c == ']');
        if trimmed.parse::<std::net::IpAddr>().is_ok() {
            return true;
        }
        // Uri.CheckHostName(...) == Dns: dot-separated labels of letters, digits,
        // hyphens and underscores, none empty, none starting or ending with '-'.
        host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ProxyWire {
    mode: ProxyMode,
    kind: ProxyKind,
    #[serde(deserialize_with = "null_as_default")]
    host: String,
    port: i32,
    #[serde(deserialize_with = "null_as_default")]
    user_name: String,
    protected_password: Option<String>,
    #[serde(deserialize_with = "null_as_default")]
    bypass: String,
    #[serde(flatten)]
    extra: Extra,
}

impl Default for ProxyWire {
    fn default() -> Self {
        ProxySettings::default().into()
    }
}

impl From<ProxyWire> for ProxySettings {
    fn from(w: ProxyWire) -> Self {
        Self {
            mode: w.mode,
            kind: w.kind,
            host: w.host,
            port: w.port,
            user_name: w.user_name,
            password: secret_protector::unprotect(w.protected_password.as_deref()),
            bypass: w.bypass,
            extra: w.extra,
        }
    }
}

impl From<ProxySettings> for ProxyWire {
    fn from(p: ProxySettings) -> Self {
        Self {
            mode: p.mode,
            kind: p.kind,
            protected_password: secret_protector::protect(&p.password),
            host: p.host,
            port: p.port,
            user_name: p.user_name,
            bypass: p.bypass,
            extra: p.extra,
        }
    }
}

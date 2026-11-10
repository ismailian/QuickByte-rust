//! Port of `src/QuickByte.Core/Services/Proxy/AppProxy.cs`.
//!
//! The one proxy every HTTP request in the engine is routed by — the downloads,
//! the probe, the playlist reader and the update check — answering from
//! whatever Options last said.
//!
//! A live object rather than a setting read at start-up, for the reason the
//! speed limit is one: the client is shared and created once. It asks this
//! object for every request ([`AppProxy::reqwest_proxy`] is a
//! `reqwest::Proxy::custom`), so answering from the current settings is all it
//! takes for a change in Options to apply to the next request.
//!
//! `ProxyMode::System` hands the question to Windows' own settings — what every
//! client did before this existed (see [`crate::platform::proxy_config`]).

use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;
use url::Url;

use crate::models::{ProxyKind, ProxyMode, ProxySettings};
use crate::net::proxy_bypass;

/// Where Windows' proxy settings come from. The real one is
/// [`crate::platform::proxy_config::WindowsProxyConfig`]; tests pass fakes,
/// because Windows' proxy settings are the machine's, not a test's.
pub trait SystemProxySource: Send + Sync {
    /// The proxy for `destination`, `None` to connect directly, or an error when
    /// the settings cannot be read.
    fn proxy_for(&self, destination: &Url) -> Result<Option<Url>, String>;
}

/// A proxy an FTP connection tunnels through; see [`crate::net::proxy_tunnel`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyEndpoint {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    pub credentials: Option<ProxyCredential>,
}

/// A user name and password for a proxy.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyCredential {
    pub user_name: String,
    pub password: String,
}

impl std::fmt::Debug for ProxyCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyCredential").field("user_name", &self.user_name).finish_non_exhaustive()
    }
}

/// What a proxy that challenges is answered with (`LiveCredentials` in C#).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyLogin {
    /// Nothing to offer.
    None,
    /// Only ever to the proxy the user named, and only what they typed.
    Manual(ProxyCredential),
    /// A proxy Windows is set to use, challenging with NTLM or Negotiate: the
    /// signed-in user's own credentials, which is what a browser sends.
    DefaultNetworkCredentials,
}

struct Snapshot {
    settings: ProxySettings,
    proxy_url: Option<Url>,
    credential: Option<ProxyCredential>,
    bypass: Vec<String>,
}

impl Snapshot {
    fn build(settings: ProxySettings) -> Self {
        let mut proxy_url = None;
        let mut credential = None;

        if settings.mode == ProxyMode::Manual && settings.is_complete() {
            let scheme = match settings.kind {
                ProxyKind::Socks5 => "socks5",
                ProxyKind::Socks4 => "socks4a",
                ProxyKind::Http => "http",
            };
            let mut host = settings.host.trim().to_string();
            if let Ok(std::net::IpAddr::V6(address)) = host.trim_matches(|c| c == '[' || c == ']').parse::<std::net::IpAddr>() {
                host = format!("[{address}]");
            }
            proxy_url = Url::parse(&format!("{scheme}://{host}:{}", settings.port)).ok();
            if !settings.user_name.is_empty() {
                credential = Some(ProxyCredential { user_name: settings.user_name.clone(), password: settings.password.clone() });
            }
        }

        let bypass = proxy_bypass::parse(Some(&settings.bypass));
        Self { settings, proxy_url, credential, bypass }
    }

    fn bypassed(&self, destination: &Url) -> bool {
        proxy_bypass::matches(&self.bypass, destination)
    }
}

pub struct AppProxy {
    system: Option<Arc<dyn SystemProxySource>>,
    current: RwLock<Arc<Snapshot>>,
    generation: AtomicU64,
}

impl AppProxy {
    /// The instance the engine's shared client uses, reading Windows' settings
    /// in `System` mode. Configured by the composition root.
    pub fn shared() -> &'static Arc<AppProxy> {
        static SHARED: OnceLock<Arc<AppProxy>> = OnceLock::new();
        SHARED.get_or_init(|| Arc::new(AppProxy::new(Some(Arc::new(crate::platform::proxy_config::WindowsProxyConfig::new())))))
    }

    /// `system` is where `ProxyMode::System` looks; `None` means "no proxy".
    pub fn new(system: Option<Arc<dyn SystemProxySource>>) -> Self {
        Self { system, current: RwLock::new(Arc::new(Snapshot::build(ProxySettings::default()))), generation: AtomicU64::new(0) }
    }

    /// Applies Options. The settings are copied, so a form editing them later changes nothing.
    pub fn configure(&self, settings: Option<&ProxySettings>) {
        let settings = settings.cloned().unwrap_or_default();
        *self.current.write() = Arc::new(Snapshot::build(settings));
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Bumped by every [`configure`](Self::configure), so a client can tell its
    /// pooled connections were made under other settings.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn mode(&self) -> ProxyMode {
        self.current.read().settings.mode
    }

    /// The proxy for `destination`, or `None` to connect directly.
    pub fn get_proxy(&self, destination: &Url) -> Option<Url> {
        if proxy_bypass::is_local(destination) {
            return None;
        }
        let current = self.current.read().clone();
        match current.settings.mode {
            ProxyMode::None => None,
            ProxyMode::Manual => match &current.proxy_url {
                Some(url) if !current.bypassed(destination) => Some(url.clone()),
                _ => None,
            },
            ProxyMode::System => {
                let system = self.system.as_ref()?;
                // An unreadable system setting is no proxy, as it was before.
                let proxy = system.proxy_for(destination).ok().flatten()?;
                (proxy != *destination).then_some(proxy)
            }
        }
    }

    pub fn is_bypassed(&self, destination: &Url) -> bool {
        self.get_proxy(destination).is_none()
    }

    /// What a proxy that challenges is answered with, looked up at the moment
    /// it asks, so a change in Options applies.
    pub fn login(&self) -> ProxyLogin {
        let current = self.current.read().clone();
        match current.settings.mode {
            ProxyMode::Manual => current.credential.clone().map(ProxyLogin::Manual).unwrap_or(ProxyLogin::None),
            ProxyMode::System => ProxyLogin::DefaultNetworkCredentials,
            ProxyMode::None => ProxyLogin::None,
        }
    }

    /// Where an FTP connection to `destination` should tunnel, or `None` to
    /// connect directly. Only a manual proxy is tunnelled through: Windows'
    /// settings name a proxy for the web, and an HTTP proxy asked to CONNECT to
    /// port 21 is one that almost always refuses.
    pub fn tunnel_for(&self, destination: &Url) -> Option<ProxyEndpoint> {
        let current = self.current.read().clone();
        if current.settings.mode != ProxyMode::Manual || current.proxy_url.is_none() {
            return None;
        }
        if proxy_bypass::is_local(destination) || current.bypassed(destination) {
            return None;
        }
        Some(ProxyEndpoint {
            kind: current.settings.kind,
            host: current.settings.host.trim().trim_matches(|c| c == '[' || c == ']').to_string(),
            port: current.settings.port as u16,
            credentials: current.credential.clone(),
        })
    }

    /// The tunnel a request that `reqwest` cannot carry itself (the signed-in
    /// connections of [`crate::net::authenticated_http`]) goes through to reach
    /// `destination`: whatever [`get_proxy`](Self::get_proxy) names, as an endpoint.
    pub fn endpoint_for(&self, destination: &Url) -> Option<ProxyEndpoint> {
        let proxy = self.get_proxy(destination)?;
        let kind = match proxy.scheme() {
            "socks5" | "socks5h" => ProxyKind::Socks5,
            "socks4" | "socks4a" => ProxyKind::Socks4,
            _ => ProxyKind::Http,
        };
        let credentials = match self.login() {
            ProxyLogin::Manual(c) => Some(c),
            _ => None,
        };
        Some(ProxyEndpoint {
            kind,
            host: proxy.host_str()?.trim_matches(|c| c == '[' || c == ']').to_string(),
            port: proxy.port_or_known_default().unwrap_or(if kind == ProxyKind::Http { 80 } else { 1080 }),
            credentials,
        })
    }

    /// The `reqwest` proxy that asks this object on every request. Host names go
    /// to a SOCKS proxy to resolve (`socks5h`, `socks4a`), so DNS lookups do not
    /// leak around it, and the manual login travels in the proxy URL — reqwest
    /// sends it as `Proxy-Authorization: Basic` to an HTTP proxy and as the
    /// RFC 1929 login to a SOCKS5 one.
    pub fn reqwest_proxy(self: &Arc<Self>) -> reqwest::Proxy {
        let this = Arc::clone(self);
        reqwest::Proxy::custom(move |destination| {
            let mut proxy = this.get_proxy(destination)?;
            if proxy.scheme() == "socks5" {
                let rewritten = proxy.as_str().replacen("socks5://", "socks5h://", 1);
                proxy = Url::parse(&rewritten).ok()?;
            }
            if let ProxyLogin::Manual(login) = this.login() {
                let _ = proxy.set_username(&percent_encoding::utf8_percent_encode(&login.user_name, USERINFO).to_string());
                let _ = proxy.set_password(Some(&percent_encoding::utf8_percent_encode(&login.password, USERINFO).to_string()));
            }
            Some(proxy)
        })
    }
}

/// What has to be escaped in a URL's user-info.
const USERINFO: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedProxy {
        proxy: Url,
        bypassed: Option<&'static str>,
    }
    impl SystemProxySource for FixedProxy {
        fn proxy_for(&self, destination: &Url) -> Result<Option<Url>, String> {
            if destination.host_str() == self.bypassed { Ok(None) } else { Ok(Some(self.proxy.clone())) }
        }
    }

    struct ThrowingProxy;
    impl SystemProxySource for ThrowingProxy {
        fn proxy_for(&self, _: &Url) -> Result<Option<Url>, String> {
            Err("registry unreadable".into())
        }
    }

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn manual(kind: ProxyKind, host: &str, user: &str, password: &str, bypass: &str) -> AppProxy {
        let proxy = AppProxy::new(None);
        proxy.configure(Some(&ProxySettings {
            mode: ProxyMode::Manual,
            kind,
            host: host.into(),
            port: 1080,
            user_name: user.into(),
            password: password.into(),
            bypass: bypass.into(),
            ..Default::default()
        }));
        proxy
    }

    fn socks5() -> AppProxy {
        manual(ProxyKind::Socks5, "proxy.example.com", "", "", "")
    }

    #[test]
    fn a_manual_proxy_is_named_with_the_scheme_the_handler_speaks() {
        for (kind, expected) in [
            (ProxyKind::Http, "http://proxy.example.com:1080/"),
            (ProxyKind::Socks5, "socks5://proxy.example.com:1080"),
            (ProxyKind::Socks4, "socks4a://proxy.example.com:1080"),
        ] {
            let proxy = manual(kind, "proxy.example.com", "", "", "");
            assert_eq!(proxy.get_proxy(&url("https://files.example.com/a.zip")).unwrap().as_str(), expected);
        }
    }

    #[test]
    fn an_ipv6_proxy_address_is_bracketed() {
        let proxy = manual(ProxyKind::Socks5, "2001:db8::7", "", "", "");
        assert_eq!(proxy.get_proxy(&url("https://files.example.com/")).unwrap().host_str(), Some("[2001:db8::7]"));
    }

    #[test]
    fn this_machine_is_never_reached_through_a_proxy() {
        let proxy = socks5();
        for target in ["http://localhost:9614/ping", "http://127.0.0.1/", "http://[::1]/", "http://app.localhost/"] {
            assert_eq!(proxy.get_proxy(&url(target)), None, "{target}");
            assert!(proxy.is_bypassed(&url(target)));
        }
    }

    #[test]
    fn the_bypass_list_sends_its_hosts_directly() {
        let proxy = manual(ProxyKind::Socks5, "proxy.example.com", "", "", "*.lan; intranet.example.com");
        assert_eq!(proxy.get_proxy(&url("http://nas.lan/share.iso")), None);
        assert_eq!(proxy.get_proxy(&url("https://intranet.example.com/")), None);
        assert!(proxy.get_proxy(&url("https://example.com/")).is_some());
    }

    #[test]
    fn no_proxy_means_none_whatever_windows_says() {
        let proxy = AppProxy::new(Some(Arc::new(FixedProxy { proxy: url("http://system-proxy:3128"), bypassed: None })));
        proxy.configure(Some(&ProxySettings { mode: ProxyMode::None, ..Default::default() }));
        assert_eq!(proxy.get_proxy(&url("https://example.com/")), None);
        assert_eq!(proxy.login(), ProxyLogin::None);
    }

    #[test]
    fn windows_settings_are_asked_and_their_answer_used() {
        let proxy = AppProxy::new(Some(Arc::new(FixedProxy { proxy: url("http://system-proxy:3128"), bypassed: Some("example.org") })));
        assert_eq!(proxy.get_proxy(&url("https://example.com/")).unwrap().as_str(), "http://system-proxy:3128/");
        assert_eq!(proxy.get_proxy(&url("https://example.org/")), None);
    }

    #[test]
    fn windows_settings_that_cannot_be_read_mean_no_proxy_rather_than_a_failed_download() {
        let proxy = AppProxy::new(Some(Arc::new(ThrowingProxy)));
        assert_eq!(proxy.get_proxy(&url("https://example.com/")), None);
    }

    #[test]
    fn the_login_goes_only_to_a_proxy_the_user_named_and_follows_a_change_in_options() {
        let proxy = manual(ProxyKind::Socks5, "proxy.example.com", "alice", "s3cret", "");
        assert_eq!(
            proxy.login(),
            ProxyLogin::Manual(ProxyCredential { user_name: "alice".into(), password: "s3cret".into() })
        );

        proxy.configure(Some(&ProxySettings { mode: ProxyMode::Manual, host: "proxy.example.com".into(), port: 1080, ..Default::default() }));
        assert_eq!(proxy.login(), ProxyLogin::None);

        proxy.configure(Some(&ProxySettings { mode: ProxyMode::System, ..Default::default() }));
        assert_eq!(proxy.login(), ProxyLogin::DefaultNetworkCredentials);
    }

    #[test]
    fn a_manual_proxy_missing_its_host_connects_directly_rather_than_nowhere() {
        let proxy = AppProxy::new(None);
        proxy.configure(Some(&ProxySettings { mode: ProxyMode::Manual, host: String::new(), port: 1080, ..Default::default() }));
        assert_eq!(proxy.get_proxy(&url("https://example.com/")), None);
        assert_eq!(proxy.tunnel_for(&url("ftp://example.com/")), None);
    }

    #[test]
    fn settings_are_copied_so_a_form_editing_them_later_changes_nothing() {
        let mut settings = ProxySettings { mode: ProxyMode::Manual, host: "proxy.example.com".into(), port: 1080, ..Default::default() };
        let proxy = AppProxy::new(None);
        proxy.configure(Some(&settings));
        settings.host = "elsewhere.example.com".into();
        assert_eq!(proxy.get_proxy(&url("https://example.com/")).unwrap().host_str(), Some("proxy.example.com"));
    }

    #[test]
    fn ftp_tunnels_only_through_a_proxy_named_in_options() {
        let ftp = url("ftp://ftp.example.com/pub/file.iso");

        let endpoint = manual(ProxyKind::Http, "proxy.example.com", "bob", "pw", "").tunnel_for(&ftp).unwrap();
        assert_eq!(
            (endpoint.kind, endpoint.host.as_str(), endpoint.port, endpoint.credentials.unwrap().user_name.as_str()),
            (ProxyKind::Http, "proxy.example.com", 1080, "bob")
        );

        let system = AppProxy::new(Some(Arc::new(FixedProxy { proxy: url("http://system-proxy:3128"), bypassed: None })));
        assert_eq!(system.tunnel_for(&ftp), None);
        assert_eq!(manual(ProxyKind::Socks5, "proxy.example.com", "", "", "ftp.example.com").tunnel_for(&ftp), None);
    }

    #[test]
    fn every_change_moves_the_generation_on() {
        let proxy = AppProxy::new(None);
        let before = proxy.generation();
        proxy.configure(None);
        assert_eq!(proxy.generation(), before + 1);
    }
}

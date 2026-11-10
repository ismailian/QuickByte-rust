//! Windows' own proxy settings — what `HttpClient.DefaultProxy` gave the C#
//! build, and what every HTTP client used before Options had a Proxy tab.
//!
//! `WinHttpGetIEProxyConfigForCurrentUser` reads the user's static proxy and
//! bypass list (the same settings Internet Options and Edge show), and when a
//! configuration script or auto-detection is switched on, `WinHttpGetProxyForUrl`
//! runs the PAC script / WPAD for the address. No registry access here: Core
//! has none, and WinHTTP reads it on our behalf.
//!
//! Both answers are cached briefly. The proxy is consulted on every request (see
//! [`crate::net::app_proxy`]), and a PAC evaluation or a WPAD lookup can take
//! long enough that doing it per request would be noticed; a change made in
//! Windows' settings still reaches QuickByte within the cache's lifetime.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use url::Url;

use crate::net::app_proxy::SystemProxySource;
use crate::net::proxy_bypass;

/// How long the user's static settings are trusted before they are read again.
const CONFIG_TTL: Duration = Duration::from_secs(30);
/// How long a PAC / WPAD answer for one origin is reused.
const AUTO_PROXY_TTL: Duration = Duration::from_secs(60);

/// The user's Internet Options proxy configuration, as WinHTTP reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IeProxyConfig {
    pub auto_detect: bool,
    pub auto_config_url: Option<String>,
    /// `host:port`, or `http=host:port;https=host:port;socks=host:port`.
    pub proxy: Option<String>,
    /// `<local>;*.corp.example.com;10.*`.
    pub bypass: Option<String>,
}

impl IeProxyConfig {
    pub fn uses_auto_proxy(&self) -> bool {
        self.auto_detect || self.auto_config_url.as_deref().is_some_and(|u| !u.trim().is_empty())
    }
}

/// Windows' proxy settings, cached. The [`SystemProxySource`] the shared
/// [`crate::net::app_proxy::AppProxy`] uses in `ProxyMode::System`.
#[derive(Default)]
pub struct WindowsProxyConfig {
    config: Mutex<Option<(Instant, Result<IeProxyConfig, String>)>>,
    auto: Mutex<HashMap<String, (Instant, Option<AutoProxy>)>>,
}

/// A PAC / WPAD answer: the proxy list, and the bypass list that came with it.
type AutoProxy = (String, Option<String>);

impl WindowsProxyConfig {
    pub fn new() -> Self {
        Self::default()
    }

    fn config(&self) -> Result<IeProxyConfig, String> {
        let mut cached = self.config.lock();
        if let Some((at, value)) = cached.as_ref()
            && at.elapsed() < CONFIG_TTL
        {
            return value.clone();
        }
        let value = read_ie_config();
        *cached = Some((Instant::now(), value.clone()));
        value
    }

    /// The PAC / WPAD answer for `destination`: the proxy list and its bypass
    /// list, `Some((list, bypass))`; or `None` when the script said DIRECT or
    /// could not be run (the static settings then decide).
    fn auto_proxy(&self, config: &IeProxyConfig, destination: &Url) -> Option<AutoProxy> {
        let key = format!("{}://{}", destination.scheme(), destination.host_str().unwrap_or(""));
        if let Some((at, answer)) = self.auto.lock().get(&key)
            && at.elapsed() < AUTO_PROXY_TTL
        {
            return answer.clone();
        }
        let answer = get_proxy_for_url(config, destination.as_str());
        let mut auto = self.auto.lock();
        if auto.len() > 256 {
            auto.clear();
        }
        auto.insert(key, (Instant::now(), answer.clone()));
        answer
    }
}

impl SystemProxySource for WindowsProxyConfig {
    fn proxy_for(&self, destination: &Url) -> Result<Option<Url>, String> {
        let config = self.config()?;

        if config.uses_auto_proxy()
            && let Some((list, bypass)) = self.auto_proxy(&config, destination)
        {
            if bypass.as_deref().is_some_and(|b| proxy_bypass::matches(&proxy_bypass::parse(Some(b)), destination)) {
                return Ok(None);
            }
            return Ok(pick_proxy(&list, destination.scheme()));
        }

        let Some(list) = config.proxy.as_deref().filter(|p| !p.trim().is_empty()) else { return Ok(None) };
        if config.bypass.as_deref().is_some_and(|b| proxy_bypass::matches(&proxy_bypass::parse(Some(b)), destination)) {
            return Ok(None);
        }
        Ok(pick_proxy(list, destination.scheme()))
    }
}

/// Chooses the proxy for `scheme` from a WinINET proxy list: a `scheme=` entry
/// for it first, else an unscoped `host:port`, else a `socks=` entry. `DIRECT`
/// (a PAC answer) is no proxy.
pub fn pick_proxy(list: &str, scheme: &str) -> Option<Url> {
    let mut unscoped = None;
    let mut socks = None;
    let mut scoped = None;
    for entry in list.split([';', ' ', '\t']).map(str::trim).filter(|e| !e.is_empty()) {
        if entry.eq_ignore_ascii_case("DIRECT") {
            if unscoped.is_none() {
                unscoped = Some(None);
            }
            continue;
        }
        // A PAC answer: "PROXY host:port", "SOCKS host:port".
        let (kind, address) = match entry.split_once('=') {
            Some((kind, address)) => (Some(kind.trim().to_ascii_lowercase()), address.trim()),
            None => (None, entry),
        };
        match kind.as_deref() {
            Some(k) if k == scheme => {
                if scoped.is_none() {
                    scoped = Some(proxy_url("http", address));
                }
            }
            Some("socks") => {
                if socks.is_none() {
                    socks = Some(proxy_url("socks4", address));
                }
            }
            Some(_) => {}
            None => {
                if unscoped.is_none() {
                    unscoped = Some(proxy_url("http", address));
                }
            }
        }
    }
    scoped.or(unscoped).or(socks).flatten()
}

fn proxy_url(default_scheme: &str, address: &str) -> Option<Url> {
    let address = address.trim().trim_end_matches('/');
    if address.is_empty() {
        return None;
    }
    let text = if address.contains("://") { address.to_string() } else { format!("{default_scheme}://{address}") };
    Url::parse(&text).ok().filter(|u| u.host_str().is_some())
}

/// PAC-style answers ("PROXY a:1; SOCKS b:2; DIRECT") reduced to the WinINET form.
fn normalise_auto_list(list: &str) -> String {
    list.split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|entry| {
            let upper = entry.to_ascii_uppercase();
            if let Some(rest) = upper.strip_prefix("PROXY ").map(|_| &entry[6..]) {
                rest.trim().to_string()
            } else if upper.starts_with("SOCKS") {
                let rest = entry.split_once(' ').map(|(_, r)| r).unwrap_or("");
                format!("socks={}", rest.trim())
            } else {
                entry.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(windows)]
fn read_ie_config() -> Result<IeProxyConfig, String> {
    use windows::Win32::Networking::WinHttp::{WINHTTP_CURRENT_USER_IE_PROXY_CONFIG, WinHttpGetIEProxyConfigForCurrentUser};

    let mut raw = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
    // SAFETY: WinHTTP fills the struct; every string it allocates is released by
    // `take_wide` with GlobalFree, as the API requires.
    unsafe {
        WinHttpGetIEProxyConfigForCurrentUser(&mut raw).map_err(|e| e.message())?;
        Ok(IeProxyConfig {
            auto_detect: raw.fAutoDetect.as_bool(),
            auto_config_url: take_wide(raw.lpszAutoConfigUrl.0),
            proxy: take_wide(raw.lpszProxy.0),
            bypass: take_wide(raw.lpszProxyBypass.0),
        })
    }
}

#[cfg(not(windows))]
fn read_ie_config() -> Result<IeProxyConfig, String> {
    Ok(IeProxyConfig::default())
}

/// Copies and frees a WinHTTP-allocated string.
#[cfg(windows)]
unsafe fn take_wide(ptr: *mut u16) -> Option<String> {
    use windows::Win32::Foundation::{GlobalFree, HGLOBAL};
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `ptr` is a NUL-terminated string WinHTTP allocated with GlobalAlloc.
    unsafe {
        let text = windows::core::PCWSTR(ptr).to_string().ok();
        let _ = GlobalFree(Some(HGLOBAL(ptr as *mut _)));
        text.filter(|t| !t.is_empty())
    }
}

#[cfg(windows)]
struct Session(*mut core::ffi::c_void);
// SAFETY: a WinHTTP session handle may be used from any thread.
#[cfg(windows)]
unsafe impl Send for Session {}
#[cfg(windows)]
unsafe impl Sync for Session {}

#[cfg(windows)]
fn session() -> Option<&'static Session> {
    use std::sync::OnceLock;
    use windows::Win32::Networking::WinHttp::{WINHTTP_ACCESS_TYPE_NO_PROXY, WinHttpOpen};
    use windows::core::{PCWSTR, w};
    static SESSION: OnceLock<Option<Session>> = OnceLock::new();
    SESSION
        .get_or_init(|| {
            // SAFETY: plain call; the handle lives for the process.
            let handle = unsafe { WinHttpOpen(w!("QuickByte"), WINHTTP_ACCESS_TYPE_NO_PROXY, PCWSTR::null(), PCWSTR::null(), 0) };
            (!handle.is_null()).then_some(Session(handle))
        })
        .as_ref()
}

#[cfg(windows)]
fn get_proxy_for_url(config: &IeProxyConfig, url: &str) -> Option<AutoProxy> {
    use windows::Win32::Networking::WinHttp::{
        WINHTTP_ACCESS_TYPE_NAMED_PROXY, WINHTTP_AUTO_DETECT_TYPE_DHCP, WINHTTP_AUTO_DETECT_TYPE_DNS_A,
        WINHTTP_AUTOPROXY_AUTO_DETECT, WINHTTP_AUTOPROXY_CONFIG_URL, WINHTTP_AUTOPROXY_OPTIONS, WINHTTP_PROXY_INFO,
        WinHttpGetProxyForUrl,
    };
    use windows::core::{HSTRING, PCWSTR};

    let session = session()?;
    let script = config.auto_config_url.as_deref().filter(|u| !u.trim().is_empty()).map(HSTRING::from);
    let mut options = WINHTTP_AUTOPROXY_OPTIONS::default();
    if let Some(script) = script.as_ref() {
        options.dwFlags |= WINHTTP_AUTOPROXY_CONFIG_URL;
        options.lpszAutoConfigUrl = PCWSTR(script.as_ptr());
    }
    if config.auto_detect {
        options.dwFlags |= WINHTTP_AUTOPROXY_AUTO_DETECT;
        options.dwAutoDetectFlags = WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A;
    }
    options.fAutoLogonIfChallenged = true.into();

    let target = HSTRING::from(url);
    let mut info = WINHTTP_PROXY_INFO::default();
    // SAFETY: every pointer handed in outlives the call; the strings WinHTTP
    // returns in `info` are released by `take_wide`.
    unsafe {
        WinHttpGetProxyForUrl(session.0, &target, &mut options, &mut info).ok()?;
        let list = take_wide(info.lpszProxy.0);
        let bypass = take_wide(info.lpszProxyBypass.0);
        if info.dwAccessType != WINHTTP_ACCESS_TYPE_NAMED_PROXY {
            return None;
        }
        list.map(|l| (normalise_auto_list(&l), bypass))
    }
}

#[cfg(not(windows))]
fn get_proxy_for_url(_config: &IeProxyConfig, _url: &str) -> Option<AutoProxy> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_proxy_serves_every_scheme() {
        assert_eq!(pick_proxy("proxy.corp:3128", "https").unwrap().as_str(), "http://proxy.corp:3128/");
    }

    #[test]
    fn a_scheme_of_its_own_is_preferred() {
        let list = "http=web.corp:80;https=secure.corp:8443;socks=s.corp:1080";
        assert_eq!(pick_proxy(list, "https").unwrap().as_str(), "http://secure.corp:8443/");
        assert_eq!(pick_proxy(list, "http").unwrap().as_str(), "http://web.corp/");
    }

    #[test]
    fn socks_is_the_last_resort() {
        assert_eq!(pick_proxy("socks=s.corp:1080", "https").unwrap().as_str(), "socks4://s.corp:1080");
        assert_eq!(pick_proxy("ftp=f.corp:21", "https"), None);
    }

    #[test]
    fn direct_is_no_proxy() {
        assert_eq!(pick_proxy("DIRECT", "https"), None);
    }

    #[test]
    fn a_pac_answer_reads_like_the_static_form() {
        assert_eq!(normalise_auto_list("PROXY a.corp:8080; SOCKS b.corp:1080; DIRECT"), "a.corp:8080;socks=b.corp:1080;DIRECT");
        assert_eq!(pick_proxy(&normalise_auto_list("PROXY a.corp:8080; DIRECT"), "https").unwrap().as_str(), "http://a.corp:8080/");
    }

    #[test]
    fn reading_the_settings_does_not_fail() {
        // Whatever this machine is set to, reading it must succeed and be cached.
        // (No proxy_for here: with auto-detect on, that would go looking for WPAD.)
        let config = WindowsProxyConfig::new();
        assert!(config.config().is_ok());
        assert!(config.config.lock().is_some());
    }
}

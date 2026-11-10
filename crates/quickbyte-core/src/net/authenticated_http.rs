//! Port of `src/QuickByte.Core/Services/AuthenticatedHttp.cs`.
//!
//! Signs a request in when the server wants more than HTTP Basic — Digest, NTLM
//! or Negotiate, the last two being what a Windows server or an intranet file
//! share usually asks for.
//!
//! Basic is still sent up front by [`crate::net::request_decorator`]: it costs
//! nothing, and every connection sharing one client could not do the
//! challenge-response on its own. The other schemes cannot be sent up front at
//! all — Digest answers a nonce the server issues, and NTLM and Negotiate are a
//! handshake bound to one TCP connection. So a request whose 401 offers one of
//! them is sent again over a connection of its own for that login and that
//! server, which does the handshake (Digest computed in [`crate::net::digest`],
//! NTLM and Negotiate produced by Windows through [`crate::net::sspi`]), and the
//! server is remembered so later requests go straight there.
//!
//! The login is registered for the server the request was made to and nowhere
//! else, so a redirect to another host is never answered with it, and a
//! challenge that came from a redirect target is not retried. Each signed-in
//! request runs on a dedicated HTTP/1.1 connection — the handshake is bound to
//! it — opened through the same proxy any other request would use.

use std::collections::HashSet;
use std::sync::OnceLock;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use futures::TryStreamExt;
use http_body_util::{BodyStream, Empty};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, COOKIE, HOST, HeaderMap, HeaderValue, LOCATION, WWW_AUTHENTICATE};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::{CoreError, Result};
use crate::models::{DownloadCredentials, RequestOptions};
use crate::net::app_proxy::AppProxy;
use crate::net::digest::{Challenge, DigestSession, parse_challenges};
use crate::net::http_client::{BodyReader, HttpClient, HttpRequest, HttpResponse, MAX_REDIRECTS, with_deadline};
use crate::net::proxy_tunnel;
use crate::net::sspi::{SspiContext, WindowsLogin};

/// The schemes Basic cannot stand in for, strongest first.
const CHALLENGE_SCHEMES: [&str; 3] = ["Negotiate", "NTLM", "Digest"];

/// A 401 body read past to keep the connection for the next leg of the handshake.
const MAX_CHALLENGE_BODY: usize = 1024 * 1024;

/// Origin and login pairs whose server has asked for a challenge-response scheme.
fn needs_challenge() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Sends `request` through `http`, or over a signed-in connection when the
/// server has asked for a challenge-response scheme.
pub async fn send(http: &HttpClient, request: &HttpRequest, options: Option<&RequestOptions>, cancel: &CancellationToken) -> Result<HttpResponse> {
    let Some(credentials) = options.and_then(|o| o.credentials.as_ref()).filter(|c| !c.is_empty()) else {
        return http.send(request, cancel).await;
    };

    let key = key(&request.url, credentials);
    if !needs_challenge().lock().contains(&key) {
        let response = http.send(request, cancel).await?;
        if !wants_challenge_response(response.status(), response.headers(), response.url(), &request.url) {
            return Ok(response);
        }
        needs_challenge().lock().insert(key);
        drop(response);
    }

    // The handshake is for a request that carries no Authorization of its own —
    // and the pre-emptive Basic one was just refused.
    let mut signed = request.clone();
    signed.headers.remove(AUTHORIZATION);
    let pending = send_signed_in(http, signed, credentials, cancel);
    with_deadline(pending, request.timeout, cancel).await
}

/// A 401, from the server the request was made to, offering a scheme the
/// handshake can answer and Basic cannot. A 401 offering only Basic is a
/// rejected password, and asking again with the same one would not change that.
pub fn wants_challenge_response(status: StatusCode, headers: &HeaderMap, answered_by: &Url, requested: &Url) -> bool {
    if status != StatusCode::UNAUTHORIZED || !same_origin(answered_by, requested) {
        return false;
    }
    let values = headers.get_all(WWW_AUTHENTICATE).iter().filter_map(|v| v.to_str().ok());
    parse_challenges(values).iter().any(|c| CHALLENGE_SCHEMES.iter().any(|s| c.is(s)))
}

/// Forgets which servers asked for a handshake. For the tests.
pub fn forget() {
    needs_challenge().lock().clear();
}

/// The origin, the user, and a hash of the password — the key never holds the password itself.
fn key(url: &Url, credentials: &DownloadCredentials) -> String {
    let secret = Sha256::digest(format!("{}\0{}", credentials.user_name, credentials.password).as_bytes());
    let hex: String = secret.iter().map(|b| format!("{b:02X}")).collect();
    format!("{}|{hex}", origin_of(url).to_ascii_lowercase())
}

/// `Uri.GetLeftPart(UriPartial.Authority)`: scheme, host and a non-default port.
fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

fn same_origin(a: &Url, b: &Url) -> bool {
    origin_of(a).eq_ignore_ascii_case(&origin_of(b))
}

/// Follows redirects within the origin over fresh signed-in connections; one
/// that leaves it goes through the shared client with the login stripped off.
async fn send_signed_in(http: &HttpClient, mut request: HttpRequest, credentials: &DownloadCredentials, cancel: &CancellationToken) -> Result<HttpResponse> {
    let origin = origin_of(&request.url);
    for _ in 0..=MAX_REDIRECTS {
        if !origin_of(&request.url).eq_ignore_ascii_case(&origin) {
            // A redirect to another host is never answered with the login.
            request.headers.remove(AUTHORIZATION);
            request.headers.remove(COOKIE);
            return http.send(&request, cancel).await;
        }

        let response = handshake(http.proxy(), &request, credentials).await?;
        let Some(next) = redirect_target(&response) else { return Ok(response) };
        if request.url.scheme() == "https" && next.scheme() == "http" {
            return Ok(response);
        }
        drop(response);
        request.url = next;
    }
    Err(CoreError::http(None, "The server redirected too many times."))
}

fn redirect_target(response: &HttpResponse) -> Option<Url> {
    if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    let location = response.header(LOCATION.as_str())?;
    response.url().join(location).ok().filter(|u| matches!(u.scheme(), "http" | "https"))
}

/// One request on a connection of its own, answering the server's challenges
/// until it lets the request through or refuses for good.
async fn handshake(proxy: &AppProxy, request: &HttpRequest, credentials: &DownloadCredentials) -> Result<HttpResponse> {
    let mut connection = Connection::open(proxy, &request.url).await?;
    let response = exchange(proxy, &mut connection, request, credentials).await?;
    // The connection lives exactly as long as the response it is carrying.
    Ok(response.holding(Box::new(connection)))
}

async fn exchange(proxy: &AppProxy, connection: &mut Connection, request: &HttpRequest, credentials: &DownloadCredentials) -> Result<HttpResponse> {
    let mut response = connection.send(request, None).await?;

    let login = WindowsLogin::from_typed(&credentials.user_name, &credentials.password);
    let mut digest: Option<DigestSession> = None;
    let mut digest_answers = 0;
    let mut sspi: Option<(String, SspiContext)> = None;

    for _ in 0..6 {
        if response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let challenges = parse_challenges(response.headers().get_all(WWW_AUTHENTICATE).iter().filter_map(|v| v.to_str().ok()));

        let authorization = if let Some((scheme, context)) = sspi.as_mut() {
            // Mid-handshake: the server's answer is a token for the scheme in progress.
            let Some(token) = challenges.iter().find(|c| c.is(scheme)).and_then(|c| c.token.as_deref()) else { return Ok(response) };
            let Ok(input) = STANDARD.decode(token) else { return Ok(response) };
            let Ok((output, _done)) = context.step(Some(&input)) else { return Ok(response) };
            if output.is_empty() {
                return Ok(response);
            }
            format!("{scheme} {}", STANDARD.encode(output))
        } else {
            let Some(chosen) = choose(&challenges) else { return Ok(response) };
            if chosen.is("Digest") {
                // A second refusal of a fresh answer is a wrong password — unless
                // the server says only that its nonce went stale.
                let stale = chosen.param("stale").is_some_and(|s| s.eq_ignore_ascii_case("true"));
                if digest_answers >= 2 || (digest_answers == 1 && !stale) {
                    return Ok(response);
                }
                let session = digest.get_or_insert_with(|| DigestSession::new(&credentials.user_name, &credentials.password));
                let Some(header) = session.answer(chosen, request.method.as_str(), &request.url) else { return Ok(response) };
                digest_answers += 1;
                header
            } else {
                let scheme = if chosen.is("Negotiate") { "Negotiate" } else { "NTLM" };
                let host = request.url.host_str().unwrap_or("").trim_matches(|c| c == '[' || c == ']');
                let Ok(mut context) = SspiContext::new(scheme, &login, &format!("HTTP/{host}")) else { return Ok(response) };
                let Ok((output, _)) = context.step(None) else { return Ok(response) };
                sspi = Some((scheme.to_string(), context));
                format!("{scheme} {}", STANDARD.encode(output))
            }
        };

        // The next leg goes on the same connection — for NTLM and Negotiate it
        // has to. One the server is closing can only carry a fresh Digest.
        let closing = response.header("connection").is_some_and(|c| c.eq_ignore_ascii_case("close"));
        if closing && sspi.is_some() {
            return Ok(response);
        }
        let reusable = !closing && drain(response).await;
        if !reusable || connection.is_closed() {
            if sspi.is_some() {
                return Err(CoreError::http(None, "The server closed the connection in the middle of signing in."));
            }
            *connection = Connection::open(proxy, &request.url).await?;
        }
        response = connection.send(request, Some(&authorization)).await?;
    }
    Ok(response)
}

/// The strongest scheme on offer that a login can answer.
fn choose(challenges: &[Challenge]) -> Option<&Challenge> {
    CHALLENGE_SCHEMES.iter().find_map(|scheme| challenges.iter().find(|c| c.is(scheme)))
}

/// Reads a refused response's body so its connection can carry the next leg.
async fn drain(response: HttpResponse) -> bool {
    matches!(response.read_at_most(MAX_CHALLENGE_BODY).await, Ok(Some(_)))
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// One HTTP/1.1 connection, through the proxy, with TLS for `https`.
struct Connection {
    sender: http1::SendRequest<Empty<Bytes>>,
}

impl Connection {
    async fn open(proxy: &AppProxy, url: &Url) -> Result<Self> {
        let host = url.host_str().ok_or_else(|| CoreError::invalid("The address names no server."))?;
        let bare = host.trim_matches(|c| c == '[' || c == ']');
        let port = url.port_or_known_default().unwrap_or(80);
        let endpoint = proxy.endpoint_for(url);
        let tcp = proxy_tunnel::connect(bare, port, endpoint.as_ref()).await?;
        let _ = tcp.set_nodelay(true);

        let stream: Box<dyn Stream> = if url.scheme() == "https" {
            let connector = native_tls::TlsConnector::new().map_err(|e| CoreError::Io(e.to_string()))?;
            let connector = tokio_native_tls::TlsConnector::from(connector);
            Box::new(connector.connect(bare, tcp).await.map_err(|e| CoreError::Io(e.to_string()))?)
        } else {
            Box::new(tcp)
        };

        let (sender, connection) =
            http1::handshake(TokioIo::new(stream)).await.map_err(|e| CoreError::http(None, crate::net::http_client::error_chain(&e)))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(Self { sender })
    }

    fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    async fn send(&mut self, request: &HttpRequest, authorization: Option<&str>) -> Result<HttpResponse> {
        let map = |e: hyper::Error| CoreError::http(None, crate::net::http_client::error_chain(&e));
        self.sender.ready().await.map_err(map)?;

        let target = match request.url.query() {
            Some(query) => format!("{}?{query}", request.url.path()),
            None => request.url.path().to_string(),
        };
        let authority = match request.url.port() {
            Some(port) => format!("{}:{port}", request.url.host_str().unwrap_or("")),
            None => request.url.host_str().unwrap_or("").to_string(),
        };

        let mut builder = hyper::Request::builder().method(request.method.clone()).uri(target);
        if let Some(headers) = builder.headers_mut() {
            *headers = request.headers.clone();
            if let Ok(host) = HeaderValue::from_str(&authority) {
                headers.insert(HOST, host);
            }
            if !headers.contains_key(reqwest::header::USER_AGENT) {
                headers.insert(reqwest::header::USER_AGENT, HeaderValue::from_static(crate::net::http_client::USER_AGENT));
            }
            match authorization.and_then(|a| HeaderValue::from_str(a).ok()) {
                Some(value) => {
                    headers.insert(AUTHORIZATION, value);
                }
                None => {
                    headers.remove(AUTHORIZATION);
                }
            }
        }
        let outgoing = builder.body(Empty::<Bytes>::new()).map_err(|e| CoreError::invalid(e.to_string()))?;

        let response = self.sender.send_request(outgoing).await.map_err(map)?;
        let (parts, body) = response.into_parts();
        let stream = BodyStream::new(body)
            .try_filter_map(|frame| async move { Ok(frame.into_data().ok()) })
            .map_err(|e| std::io::Error::other(crate::net::http_client::error_chain(&e)));
        let reader: BodyReader = Box::pin(tokio_util::io::StreamReader::new(stream));
        Ok(HttpResponse::from_parts(parts.status, parts.headers, request.url.clone(), reader))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(challenges: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for c in challenges {
            map.append(WWW_AUTHENTICATE, HeaderValue::from_str(c).unwrap());
        }
        map
    }

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn a_challenge_that_came_from_another_host_is_not_answered_with_the_login() {
        // A redirect to a third party that asks for NTLM must not be handed the
        // user's credentials for the server they were typed for.
        let answered_by = url("https://elsewhere.example.net/file");
        let challenge = headers(&["NTLM"]);
        assert!(!wants_challenge_response(StatusCode::UNAUTHORIZED, &challenge, &answered_by, &url("https://files.example.com/file")));
        assert!(wants_challenge_response(StatusCode::UNAUTHORIZED, &challenge, &answered_by, &url("https://elsewhere.example.net/other")));
    }

    #[test]
    fn a_401_offering_only_basic_is_a_rejected_password_not_a_handshake() {
        let here = url("https://files.example.com/");
        assert!(!wants_challenge_response(StatusCode::UNAUTHORIZED, &headers(&["Basic realm=\"files\""]), &here, &here));
    }

    #[test]
    fn only_a_401_is_a_challenge() {
        let here = url("https://files.example.com/");
        assert!(!wants_challenge_response(StatusCode::FORBIDDEN, &headers(&["NTLM"]), &here, &here));
        assert!(wants_challenge_response(StatusCode::UNAUTHORIZED, &headers(&["Digest realm=\"x\", nonce=\"y\""]), &here, &here));
    }

    #[test]
    fn the_key_never_holds_the_password() {
        let k = key(&url("https://Files.Example.com:8443/a"), &DownloadCredentials::new("alice", "hunter2"));
        assert!(k.starts_with("https://files.example.com:8443|"));
        assert!(!k.contains("hunter2"));
    }

    #[test]
    fn the_strongest_scheme_is_chosen() {
        let challenges = parse_challenges(["Digest realm=\"r\", nonce=\"n\"", "NTLM", "Basic realm=\"r\""]);
        assert!(choose(&challenges).unwrap().is("NTLM"));
    }
}

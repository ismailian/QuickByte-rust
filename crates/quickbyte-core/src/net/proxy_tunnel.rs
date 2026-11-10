//! Port of `src/QuickByte.Core/Services/Proxy/ProxyTunnel.cs`.
//!
//! A TCP connection to a host, opened through a proxy when one is configured —
//! for FTP, which the HTTP client's own proxy support never sees: its control
//! channel and every passive data connection are raw sockets. (The signed-in
//! HTTP connections of [`crate::net::authenticated_http`] use it too.)
//!
//! Three handshakes: SOCKS5 (RFC 1928, with RFC 1929 user name and password),
//! SOCKS4a, and HTTP `CONNECT`. Host names are handed to the proxy rather than
//! resolved here, so the lookup happens where the connection does and does not
//! leak around the proxy. Every reply is read to exactly its own length: the FTP
//! server's greeting can arrive in the same packet as the proxy's answer, and a
//! byte read too many is a greeting lost.

use std::net::IpAddr;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::{CoreError, Result};
use crate::models::ProxyKind;
use crate::net::app_proxy::{ProxyCredential, ProxyEndpoint};

/// An HTTP proxy's answer to CONNECT is a status line and a few headers; past this it is not one.
const MAX_CONNECT_RESPONSE_BYTES: usize = 16 * 1024;

fn io(message: impl Into<String>) -> CoreError {
    CoreError::Io(message.into())
}

fn needed() -> CoreError {
    CoreError::ProxyAuthentication(CoreError::PROXY_AUTH_NEEDED.into())
}

fn rejected() -> CoreError {
    CoreError::ProxyAuthentication(CoreError::PROXY_AUTH_REJECTED.into())
}

/// Connects to `host:port`, through `proxy` when there is one. The caller bounds
/// the time it may take (the FTP channel gives it 30 s, handshake included).
pub async fn connect(host: &str, port: u16, proxy: Option<&ProxyEndpoint>) -> Result<TcpStream> {
    let bare = host.trim_matches(|c| c == '[' || c == ']');
    let Some(proxy) = proxy else {
        return TcpStream::connect((bare, port)).await.map_err(|e| io(e.to_string()));
    };

    let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
        .await
        .map_err(|e| io(format!("Could not reach the proxy {}:{}: {e}", proxy.host, proxy.port)))?;
    let _ = stream.set_nodelay(true);

    match proxy.kind {
        ProxyKind::Socks5 => socks5(&mut stream, host, port, proxy.credentials.as_ref()).await?,
        ProxyKind::Socks4 => socks4a(&mut stream, host, port, proxy.credentials.as_ref()).await?,
        ProxyKind::Http => http_connect(&mut stream, host, port, proxy.credentials.as_ref()).await?,
    }
    Ok(stream)
}

// ------------------------------------------------------------------ SOCKS5 --

async fn socks5(stream: &mut TcpStream, host: &str, port: u16, credentials: Option<&ProxyCredential>) -> Result<()> {
    let offer_password = credentials.is_some_and(|c| !c.user_name.is_empty());
    let greeting: &[u8] = if offer_password { &[5, 2, 0x00, 0x02] } else { &[5, 1, 0x00] };
    write(stream, greeting).await?;

    let choice = read_exact(stream, 2).await?;
    if choice[0] != 5 {
        return Err(io("The proxy did not answer as a SOCKS5 proxy."));
    }
    match choice[1] {
        0x00 => {}
        0x02 if offer_password => socks5_login(stream, credentials.expect("offered only with a login")).await?,
        0x02 => return Err(needed()),
        _ => {
            return Err(if offer_password {
                CoreError::ProxyAuthentication("The SOCKS5 proxy accepts none of the ways QuickByte can sign in.".into())
            } else {
                needed()
            });
        }
    }

    let mut request = vec![5, 1, 0]; // version, CONNECT, reserved
    write_socks5_address(&mut request, host)?;
    request.extend_from_slice(&port.to_be_bytes());
    write(stream, &request).await?;

    let head = read_exact(stream, 4).await?;
    if head[0] != 5 {
        return Err(io("The proxy did not answer as a SOCKS5 proxy."));
    }
    if head[1] != 0 {
        return Err(io(format!("The SOCKS5 proxy could not connect to {host}:{port}: {}.", socks5_reason(head[1]))));
    }

    // The address the proxy bound: read past, to exactly its length.
    let address_length = match head[3] {
        1 => 4,
        4 => 16,
        3 => read_exact(stream, 1).await?[0] as usize,
        _ => return Err(io("The SOCKS5 proxy answered with an address type that does not exist.")),
    };
    read_exact(stream, address_length + 2).await?;
    Ok(())
}

async fn socks5_login(stream: &mut TcpStream, credentials: &ProxyCredential) -> Result<()> {
    let user = credentials.user_name.as_bytes();
    let password = credentials.password.as_bytes();
    if user.len() > 255 || password.len() > 255 {
        return Err(CoreError::ProxyAuthentication(
            "The proxy's user name or password is too long for SOCKS5 (255 bytes each).".into(),
        ));
    }
    let mut login = Vec::with_capacity(3 + user.len() + password.len());
    login.push(1);
    login.push(user.len() as u8);
    login.extend_from_slice(user);
    login.push(password.len() as u8);
    login.extend_from_slice(password);
    write(stream, &login).await?;

    let status = read_exact(stream, 2).await?;
    if status[1] != 0 {
        return Err(rejected());
    }
    Ok(())
}

fn write_socks5_address(request: &mut Vec<u8>, host: &str) -> Result<()> {
    match host.trim_matches(|c| c == '[' || c == ']').parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => {
            request.push(1);
            request.extend_from_slice(&address.octets());
        }
        Ok(IpAddr::V6(address)) => {
            request.push(4);
            request.extend_from_slice(&address.octets());
        }
        Err(_) => {
            let name = ascii_host(host);
            if name.is_empty() || name.len() > 255 {
                return Err(io(format!("'{host}' is not a host name a SOCKS5 proxy can be asked for.")));
            }
            request.push(3);
            request.push(name.len() as u8);
            request.extend_from_slice(name.as_bytes());
        }
    }
    Ok(())
}

/// The IDNA (punycode) form of a host name, as `IdnMapping.GetAscii` gave it.
fn ascii_host(host: &str) -> String {
    match url::Host::parse(host) {
        Ok(url::Host::Domain(domain)) => domain,
        _ => host.to_string(),
    }
}

fn socks5_reason(code: u8) -> String {
    match code {
        1 => "the proxy failed".into(),
        2 => "its rules do not allow it".into(),
        3 => "the network is unreachable".into(),
        4 => "the host is unreachable".into(),
        5 => "the connection was refused".into(),
        6 => "it timed out".into(),
        7 => "the proxy does not support connecting".into(),
        8 => "the proxy does not support that kind of address".into(),
        _ => format!("error {code}"),
    }
}

// ----------------------------------------------------------------- SOCKS4a --

async fn socks4a(stream: &mut TcpStream, host: &str, port: u16, credentials: Option<&ProxyCredential>) -> Result<()> {
    let mut request = vec![4, 1];
    request.extend_from_slice(&port.to_be_bytes());

    let address = host.trim_matches(|c| c == '[' || c == ']').parse::<IpAddr>().ok();
    match address {
        Some(IpAddr::V6(_)) => return Err(io("A SOCKS4 proxy cannot connect to an IPv6 address.")),
        Some(IpAddr::V4(v4)) => request.extend_from_slice(&v4.octets()),
        // 0.0.0.x (x ≠ 0) is 4a's signal that a host name follows the user id.
        None => request.extend_from_slice(&[0, 0, 0, 1]),
    }
    request.extend_from_slice(credentials.map(|c| c.user_name.as_bytes()).unwrap_or_default());
    request.push(0);
    if address.is_none() {
        request.extend_from_slice(ascii_host(host).as_bytes());
        request.push(0);
    }
    write(stream, &request).await?;

    let reply = read_exact(stream, 8).await?;
    match reply[1] {
        0x5A => Ok(()),
        0x5C | 0x5D => Err(rejected()),
        _ => Err(io(format!("The SOCKS4 proxy could not connect to {host}:{port}."))),
    }
}

// ------------------------------------------------------------ HTTP CONNECT --

async fn http_connect(stream: &mut TcpStream, host: &str, port: u16, credentials: Option<&ProxyCredential>) -> Result<()> {
    let bare = host.trim_matches(|c| c == '[' || c == ']');
    let authority = match bare.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => format!("[{bare}]:{port}"),
        _ => format!("{host}:{port}"),
    };

    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    let with_login = credentials.filter(|c| !c.user_name.is_empty());
    if let Some(login) = with_login {
        let pair = STANDARD.encode(format!("{}:{}", login.user_name, login.password));
        request.push_str(&format!("Proxy-Authorization: Basic {pair}\r\n"));
    }
    request.push_str("\r\n");
    write(stream, request.as_bytes()).await?;

    let status_line = read_connect_response(stream).await?;
    let parts: Vec<&str> = status_line.splitn(3, ' ').collect();
    let status = match parts.as_slice() {
        [version, code, ..] if version.starts_with("HTTP/") && code.bytes().all(|b| b.is_ascii_digit()) => {
            code.parse::<u16>().map_err(|_| io("The proxy did not answer as an HTTP proxy."))?
        }
        _ => return Err(io("The proxy did not answer as an HTTP proxy.")),
    };

    if (200..300).contains(&status) {
        return Ok(());
    }
    if status == 407 {
        return Err(if with_login.is_some() { rejected() } else { needed() });
    }
    Err(io(format!("The proxy would not connect to {authority}: {status_line}")))
}

/// The response head, a byte at a time so nothing after its blank line is
/// consumed. Returns the status line.
async fn read_connect_response(stream: &mut TcpStream) -> Result<String> {
    let mut head = Vec::with_capacity(256);
    let mut one = [0u8; 1];
    loop {
        let read = stream.read(&mut one).await.map_err(|e| io(e.to_string()))?;
        if read == 0 {
            return Err(io("The proxy closed the connection."));
        }
        head.push(one[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() >= MAX_CONNECT_RESPONSE_BYTES {
            return Err(io("The proxy's answer never ended."));
        }
    }
    let text = String::from_utf8_lossy(&head);
    Ok(text.split('\r').next().unwrap_or("").to_string())
}

async fn write(stream: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    stream.write_all(bytes).await.map_err(|e| io(e.to_string()))
}

async fn read_exact(stream: &mut TcpStream, count: usize) -> Result<Vec<u8>> {
    let mut buffer = vec![0u8; count];
    match stream.read_exact(&mut buffer).await {
        Ok(_) => Ok(buffer),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(io("The proxy closed the connection.")),
        Err(e) => Err(io(e.to_string())),
    }
}

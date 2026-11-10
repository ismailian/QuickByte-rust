//! Port of `src/QuickByte.Core/Services/Ftp/FtpControlChannel.cs`.
//!
//! A single FTP control connection plus the one data connection it is currently
//! running: connect, log in, ask for metadata, and open a binary read at a byte
//! offset.
//!
//! Written directly on a TCP socket rather than through a library FTP client:
//! the C# build's BCL client hid exactly the two things a download manager needs
//! most — which passive port a segment is actually using, and whether the server
//! honoured `REST` — without which resume silently writes the wrong bytes into a
//! chunk file.
//!
//! One channel serves one transfer. Segmented downloads open several, each with
//! its own control connection and its own `REST` offset, because FTP has no
//! equivalent of an HTTP Range header: the only way to bound a segment is to
//! start it at an offset and stop reading at the end of it.

use std::pin::Pin;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::{CoreError, Result};
use crate::ftp::reply::FtpReply;
use crate::models::DownloadCredentials;
use crate::net::app_proxy::{AppProxy, ProxyEndpoint};
use crate::net::http_client::with_deadline;
use crate::net::proxy_tunnel;
use crate::serde_compat::DateTimeOffset;

const DEFAULT_PORT: u16 = 21;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// What anonymous FTP wants in the password field: any e-mail-shaped string.
const ANONYMOUS_USER: &str = "anonymous";
const ANONYMOUS_PASSWORD: &str = "quickbyte@example.com";

/// A byte stream the channel reads and writes: TCP, or TLS over it.
pub trait FtpStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> FtpStream for T {}

/// The data connection of a transfer: it ends when the server closes it.
pub type DataStream = Pin<Box<dyn AsyncRead + Send>>;

pub struct FtpControlChannel {
    stream: Option<Box<dyn FtpStream>>,
    /// Bytes read past the end of the last reply line.
    pending: Vec<u8>,
    host: String,
    secure: bool,
    remote_is_ipv6: bool,
    /// The proxy the control connection went through; every data connection goes the same way.
    proxy: Option<ProxyEndpoint>,
    protect_data: bool,
}

fn io(message: impl Into<String>) -> CoreError {
    CoreError::Io(message.into())
}

async fn cancellable<T>(future: impl std::future::Future<Output = Result<T>>, cancel: &CancellationToken) -> Result<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(CoreError::Cancelled),
        result = future => result,
    }
}

async fn tls(host: &str, stream: Box<dyn FtpStream>) -> Result<Box<dyn FtpStream>> {
    let connector = native_tls::TlsConnector::new().map_err(|e| io(e.to_string()))?;
    let connector = tokio_native_tls::TlsConnector::from(connector);
    let secured = connector.connect(host, stream).await.map_err(|e| io(format!("The FTP server's TLS handshake failed: {e}")))?;
    Ok(Box::new(secured))
}

impl FtpControlChannel {
    /// Opens the control connection — through the proxy Options names, if any —
    /// negotiates TLS for an `ftps://` URL, logs in (anonymously when
    /// `credentials` is `None` or empty) and switches to binary mode.
    ///
    /// `proxy` is where the proxy decision comes from; [`AppProxy::shared`]
    /// unless a test says otherwise.
    pub async fn connect(
        url: &Url,
        credentials: Option<&DownloadCredentials>,
        cancel: &CancellationToken,
        proxy: Option<&AppProxy>,
    ) -> Result<Self> {
        let secure = url.scheme().eq_ignore_ascii_case("ftps");
        let port = url.port().filter(|p| *p > 0).unwrap_or(DEFAULT_PORT);
        let host = url.host_str().ok_or_else(|| CoreError::invalid("The address names no server."))?.to_string();
        let tunnel = proxy.unwrap_or(AppProxy::shared().as_ref()).tunnel_for(url);

        // A socket has no connect timeout of its own; the deadline gives one
        // without leaving a half-open socket behind on an unreachable host, which
        // is the usual way an FTP URL fails. It covers the proxy's handshake too.
        let tcp = with_deadline(proxy_tunnel::connect(&host, port, tunnel.as_ref()), Some(CONNECT_TIMEOUT), cancel).await?;
        let remote_is_ipv6 = tcp.peer_addr().is_ok_and(|a| a.is_ipv6());

        let mut channel = Self {
            stream: Some(Box::new(tcp)),
            pending: Vec::new(),
            host: host.trim_matches(|c| c == '[' || c == ']').to_string(),
            secure,
            remote_is_ipv6,
            proxy: tunnel,
            protect_data: false,
        };
        cancellable(channel.handshake(credentials, cancel), cancel).await?;
        Ok(channel)
    }

    async fn handshake(&mut self, credentials: Option<&DownloadCredentials>, cancel: &CancellationToken) -> Result<()> {
        let greeting = self.read_reply().await?;
        if greeting.code != 220 {
            return Err(io(format!("The FTP server refused the connection: {}", greeting.text)));
        }

        if self.secure {
            self.negotiate_tls(cancel).await?;
        }

        // Best-effort, before any path is sent: without it a server defaulting to
        // Latin-1 mis-reads a non-ASCII file name and answers 550 for a file that
        // is plainly there. A server that doesn't know OPTS simply says no.
        self.send("OPTS UTF8 ON").await?;

        let named = credentials.is_some_and(|c| !c.user_name.is_empty());
        let (user, password) = match credentials {
            Some(c) if named => (c.user_name.as_str(), c.password.as_str()),
            _ => (ANONYMOUS_USER, ANONYMOUS_PASSWORD),
        };

        let mut reply = self.send(&format!("USER {user}")).await?;

        // 331 is the common path (password wanted); 230 means the server logged us
        // in on the user name alone and there is nothing left to send.
        if reply.code == 331 {
            reply = self.send(&format!("PASS {password}")).await?;
        }

        if reply.code != 230 {
            if reply.is_authentication_failure() || reply.code == 331 || reply.code == 332 {
                let supplied = credentials.is_some_and(|c| !c.is_empty());
                return Err(CoreError::auth(
                    if supplied {
                        format!("The FTP server rejected that user name or password ({}).", reply.summarize())
                    } else {
                        "This FTP server does not allow anonymous access — a user name and password are needed.".to_string()
                    },
                    supplied,
                ));
            }
            return Err(io(format!("FTP login failed: {}", reply.summarize())));
        }

        let kind = self.send("TYPE I").await?;
        if !kind.is_positive() {
            return Err(io(format!("The FTP server refused binary mode: {}", kind.summarize())));
        }
        Ok(())
    }

    async fn negotiate_tls(&mut self, _cancel: &CancellationToken) -> Result<()> {
        let auth = self.send("AUTH TLS").await?;
        if !auth.is_positive() {
            return Err(io(format!("The FTP server does not support TLS: {}", auth.summarize())));
        }
        // Nothing the server sent before its 234 may leak into the TLS stream.
        self.pending.clear();
        let plain = self.stream.take().ok_or_else(|| io("The FTP connection is closed."))?;
        self.stream = Some(tls(&self.host, plain).await?);

        // PBSZ 0 then PROT P is the fixed incantation that puts the *data* channel
        // under TLS too. Without it the login is encrypted and the file still
        // crosses the network in the clear.
        self.send("PBSZ 0").await?;
        let prot = self.send("PROT P").await?;
        self.protect_data = prot.is_positive();
        Ok(())
    }

    // ---------------------------------------------------------- metadata --

    /// File size in bytes, or 0 when the server won't answer `SIZE`.
    pub async fn get_size(&mut self, path: &str, cancel: &CancellationToken) -> Result<i64> {
        let reply = cancellable(self.send(&format!("SIZE {path}")), cancel).await?;
        if reply.code != 213 {
            return Ok(0);
        }
        let digits = reply.text.get(3..).unwrap_or("").trim();
        Ok(if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) { digits.parse().unwrap_or(0) } else { 0 })
    }

    pub async fn get_last_modified(&mut self, path: &str, cancel: &CancellationToken) -> Result<Option<DateTimeOffset>> {
        let reply = cancellable(self.send(&format!("MDTM {path}")), cancel).await?;
        if reply.code != 213 {
            return Ok(None);
        }
        let stamp = reply.text.get(3..).unwrap_or("").trim();
        Ok(chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d%H%M%S").ok().map(|n| DateTimeOffset::from_utc(n.and_utc())))
    }

    /// Whether the server advertises restartable stream transfers, which is what
    /// decides between a segmented download and a single connection.
    ///
    /// A server that doesn't implement `FEAT` at all is assumed to support
    /// `REST`: it predates the extension list, and restart has been in the base
    /// protocol since RFC 959. If that assumption is wrong the transfer still
    /// recovers — [`open_read`](Self::open_read) reports the refusal and the
    /// connection restarts from zero rather than writing misplaced bytes.
    pub async fn supports_restart(&mut self, cancel: &CancellationToken) -> Result<bool> {
        let reply = cancellable(self.send("FEAT"), cancel).await?;
        if reply.code != 211 {
            return Ok(true);
        }
        Ok(reply.text.to_ascii_uppercase().contains("REST STREAM"))
    }

    // ---------------------------------------------------------- transfer --

    /// Opens a binary read of `path` starting at `offset`. The returned stream
    /// ends when the server closes the data connection; a caller that wants only
    /// part of the file simply stops reading and drops this channel.
    ///
    /// Fails with [`CoreError::FtpRestartNotSupported`] when `offset` is non-zero
    /// and the server refused `REST`.
    pub async fn open_read(&mut self, path: &str, offset: i64, cancel: &CancellationToken) -> Result<DataStream> {
        cancellable(self.open_read_inner(path, offset, cancel), cancel).await
    }

    async fn open_read_inner(&mut self, path: &str, offset: i64, cancel: &CancellationToken) -> Result<DataStream> {
        let data = self.open_data_connection(cancel).await?;

        if offset > 0 {
            let rest = self.send(&format!("REST {offset}")).await?;
            if rest.code != 350 {
                return Err(CoreError::FtpRestartNotSupported);
            }
        }

        let retr = self.send(&format!("RETR {path}")).await?;

        // 125/150 mean the data connection is live and the bytes are coming.
        // Anything else — 550 for a missing file, 425 for a data connection the
        // server could not open — means there will never be any.
        if !retr.is_preliminary() {
            return Err(io(format!("The FTP server refused the transfer: {}", retr.summarize())));
        }
        Ok(data)
    }

    async fn open_data_connection(&mut self, cancel: &CancellationToken) -> Result<DataStream> {
        let (host, port) = self.enter_passive_mode().await?;
        let tcp = with_deadline(proxy_tunnel::connect(&host, port, self.proxy.as_ref()), Some(CONNECT_TIMEOUT), cancel).await?;
        let stream: Box<dyn FtpStream> = Box::new(tcp);
        let stream = if self.secure && self.protect_data { tls(&self.host, stream).await? } else { stream };
        Ok(Box::pin(stream))
    }

    async fn enter_passive_mode(&mut self) -> Result<(String, u16)> {
        // EPSV first over IPv6, where PASV's four-octet address simply cannot be
        // expressed. Everywhere else PASV goes first because it is the one every
        // server implements.
        let prefer_extended = self.remote_is_ipv6;
        if prefer_extended && let Some(port) = self.try_extended_passive().await? {
            return Ok((self.host.clone(), port));
        }

        let pasv = self.send("PASV").await?;
        if pasv.code == 227
            && let Some(port) = parse_pasv_port(&pasv.text)
        {
            // The advertised IP is deliberately ignored in favour of the host the
            // control connection already reached: a server behind NAT routinely
            // announces its private address here, and dialling that is the
            // classic "passive mode hangs" failure.
            return Ok((self.host.clone(), port));
        }

        if !prefer_extended && let Some(port) = self.try_extended_passive().await? {
            return Ok((self.host.clone(), port));
        }

        Err(io(format!("The FTP server would not open a passive data connection: {}", pasv.summarize())))
    }

    async fn try_extended_passive(&mut self) -> Result<Option<u16>> {
        let epsv = self.send("EPSV").await?;
        if epsv.code != 229 {
            return Ok(None);
        }
        Ok(parse_epsv_port(&epsv.text))
    }

    // ---------------------------------------------------------- plumbing --

    async fn send(&mut self, command: &str) -> Result<FtpReply> {
        let stream = self.stream.as_mut().ok_or_else(|| io("The FTP connection is closed."))?;
        stream.write_all(format!("{command}\r\n").as_bytes()).await.map_err(|e| io(e.to_string()))?;
        stream.flush().await.map_err(|e| io(e.to_string()))?;
        self.read_reply().await
    }

    async fn read_line(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(newline) = self.pending.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.pending.drain(..=newline).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            let stream = self.stream.as_mut().ok_or_else(|| io("The FTP connection is closed."))?;
            let mut chunk = [0u8; 1024];
            let read = stream.read(&mut chunk).await.map_err(|e| io(e.to_string()))?;
            if read == 0 {
                if self.pending.is_empty() {
                    return Ok(None);
                }
                let line = String::from_utf8_lossy(&self.pending).into_owned();
                self.pending.clear();
                return Ok(Some(line));
            }
            self.pending.extend_from_slice(&chunk[..read]);
        }
    }

    /// Reads one reply, joining the continuation lines of a multi-line block. A
    /// block opens with `NNN-` and closes with the same code followed by a space,
    /// so `FEAT`'s feature list arrives as a single searchable string.
    async fn read_reply(&mut self) -> Result<FtpReply> {
        let line = self.read_line().await?.ok_or_else(|| io("The FTP server closed the connection."))?;
        let code = match line.get(..3) {
            Some(digits) if line.len() >= 4 && digits.bytes().all(|b| b.is_ascii_digit()) => digits.parse::<u16>().unwrap_or(0),
            _ => return Err(io(format!("Unexpected reply from the FTP server: {line}"))),
        };
        if line.as_bytes()[3] != b'-' {
            return Ok(FtpReply::new(code, line));
        }

        let terminator = format!("{} ", &line[..3]);
        let mut text = line;
        while let Some(next) = self.read_line().await? {
            text.push('\n');
            text.push_str(&next);
            if next.starts_with(&terminator) {
                break;
            }
        }
        Ok(FtpReply::new(code, text))
    }
}

/// The port in `227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)`.
pub fn parse_pasv_port(text: &str) -> Option<u16> {
    let open = text.find('(')?;
    let close = text[open..].find(')')? + open;
    let numbers: Vec<u32> = text[open + 1..close].split(',').map(|n| n.trim().parse().ok()).collect::<Option<_>>()?;
    if numbers.len() != 6 || numbers.iter().any(|n| *n > 255) {
        return None;
    }
    Some(((numbers[4] << 8) + numbers[5]) as u16)
}

/// The port in `229 Entering Extended Passive Mode (|||6446|)`.
pub fn parse_epsv_port(text: &str) -> Option<u16> {
    let start = text.find("(|||")? + 4;
    let end = text[start..].find("|)")? + start;
    text[start..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passive_replies_name_their_port() {
        assert_eq!(parse_pasv_port("227 Entering Passive Mode (10,0,0,9,195,80)"), Some(195 * 256 + 80));
        assert_eq!(parse_pasv_port("227 Entering Passive Mode 10,0,0,9,195,80"), None);
        assert_eq!(parse_epsv_port("229 Entering Extended Passive Mode (|||6446|)"), Some(6446));
        assert_eq!(parse_epsv_port("229 nope"), None);
    }
}

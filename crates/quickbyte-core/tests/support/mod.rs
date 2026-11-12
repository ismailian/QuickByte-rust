//! Local servers for the networking tests — the Rust shape of the C# suite's
//! `StubWeb`, `StubServer`, `AuthServer` and `ProxyFakes`.
//!
//! Everything binds 127.0.0.1 on an ephemeral port; nothing here ever reaches
//! another host. The servers answer from a script the test writes, record what
//! they were asked, and can misbehave on demand: answer a range with the whole
//! file, promise more bytes than they send, hang up without a word.

#![allow(dead_code)]

use std::sync::Arc;

use parking_lot::Mutex;
use quickbyte_core::net::app_proxy::AppProxy;
use quickbyte_core::net::http_client::HttpClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// A client of its own with no proxy at all, so the machine's settings never
/// reach a test.
pub fn direct_client() -> Arc<HttpClient> {
    HttpClient::new(Arc::new(AppProxy::new(None)))
}

/// A scratch directory that deletes itself.
pub struct TempFolder {
    pub path: std::path::PathBuf,
}

impl TempFolder {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join("QuickByte.Tests").join(uuid::Uuid::new_v4().simple().to_string());
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
    pub fn file(&self, name: &str) -> String {
        self.path.join(name).to_string_lossy().into_owned()
    }
    pub fn write(&self, name: &str, bytes: &[u8]) -> String {
        let path = self.file(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// One request as the server saw it.
#[derive(Clone, Debug)]
pub struct Asked {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    /// What came after the head, as its Content-Length said.
    pub body: Vec<u8>,
}

impl Asked {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
    }
    pub fn headers_named(&self, name: &str) -> Vec<String> {
        self.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone()).collect()
    }
}

/// What the server sends back.
#[derive(Clone, Debug, Default)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// A Content-Length other than the body's: more is promised than is sent,
    /// and the connection is closed after the body.
    pub declared_length: Option<u64>,
    /// Close the connection without answering at all.
    pub hang_up: bool,
}

impl Reply {
    pub fn status(status: u16) -> Self {
        Self { status, ..Default::default() }
    }
    pub fn ok(body: &[u8]) -> Self {
        Self { status: 200, body: body.to_vec(), ..Default::default() }
    }
    /// A 206 carrying `body` as bytes `from..=to` of a `total`-byte file.
    pub fn partial(body: &[u8], from: u64, to: u64, total: u64) -> Self {
        Self::status(206).with_body(body).header("Content-Range", &format!("bytes {from}-{to}/{total}"))
    }
    pub fn hang_up() -> Self {
        Self { hang_up: true, ..Default::default() }
    }
    pub fn redirect(location: &str) -> Self {
        Self::status(302).header("Location", location)
    }
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
    pub fn with_body(mut self, body: &[u8]) -> Self {
        self.body = body.to_vec();
        self
    }
    pub fn declared(mut self, length: u64) -> Self {
        self.declared_length = Some(length);
        self
    }
}

type Handler = dyn Fn(&Asked, usize) -> Reply + Send + Sync;

/// One entry of a script: how the server answers that request.
pub type Step = Box<dyn Fn(&Asked) -> Reply + Send + Sync>;

/// An HTTP/1.1 server with keep-alive whose answers the test writes.
pub struct TestServer {
    pub port: u16,
    requests: Arc<Mutex<Vec<Asked>>>,
    stop: CancellationToken,
}

impl TestServer {
    /// `respond` is handed each request and its index (0 for the first).
    pub async fn start(respond: impl Fn(&Asked, usize) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = CancellationToken::new();
        let handler: Arc<Handler> = Arc::new(respond);
        {
            let requests = Arc::clone(&requests);
            let stop = stop.clone();
            tokio::spawn(async move {
                loop {
                    let accepted = tokio::select! {
                        _ = stop.cancelled() => return,
                        accepted = listener.accept() => accepted,
                    };
                    let Ok((stream, _)) = accepted else { continue };
                    let requests = Arc::clone(&requests);
                    let handler = Arc::clone(&handler);
                    tokio::spawn(serve(stream, requests, handler));
                }
            });
        }
        Self { port, requests, stop }
    }

    /// A script, in order; the last entry answers every request after it.
    pub async fn script(replies: Vec<Step>) -> Self {
        Self::start(move |asked, index| replies[index.min(replies.len() - 1)](asked)).await
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub fn authority(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    pub fn requests(&self) -> Vec<Asked> {
        self.requests.lock().clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut one = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut one).await.ok()? == 0 {
            return None;
        }
        head.push(one[0]);
    }
    Some(String::from_utf8_lossy(&head).into_owned())
}

fn parse_head(head: &str) -> Asked {
    let mut lines = head.split("\r\n");
    let request_line: Vec<&str> = lines.next().unwrap_or("").split(' ').collect();
    let headers = lines
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect();
    Asked {
        method: request_line.first().unwrap_or(&"").to_string(),
        path: request_line.get(1).unwrap_or(&"").to_string(),
        headers,
        body: Vec::new(),
    }
}

async fn serve(mut stream: TcpStream, requests: Arc<Mutex<Vec<Asked>>>, handler: Arc<Handler>) {
    loop {
        let Some(head) = read_head(&mut stream).await else { return };
        let mut asked = parse_head(&head);
        // Read what the head promised, so a POST does not leave its body to be
        // misread as the next request on the connection.
        let length = asked.header("Content-Length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
        if length > 0 {
            let mut body = vec![0u8; length];
            if stream.read_exact(&mut body).await.is_err() {
                return;
            }
            asked.body = body;
        }
        let index = {
            let mut all = requests.lock();
            all.push(asked.clone());
            all.len() - 1
        };
        let reply = handler(&asked, index);
        if reply.hang_up {
            return;
        }
        let reason = http::StatusCode::from_u16(reply.status).ok().and_then(|s| s.canonical_reason()).unwrap_or("Unknown");
        let mut text = format!("HTTP/1.1 {} {reason}\r\n", reply.status);
        for (name, value) in &reply.headers {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
        let declared = reply.declared_length.unwrap_or(reply.body.len() as u64);
        text.push_str(&format!("Content-Length: {declared}\r\n\r\n"));
        if stream.write_all(text.as_bytes()).await.is_err() {
            return;
        }
        if asked.method != "HEAD" && !reply.body.is_empty() && stream.write_all(&reply.body).await.is_err() {
            return;
        }
        let _ = stream.flush().await;
        if asked.method != "HEAD" && declared != reply.body.len() as u64 {
            // Promised more than it sent: the only honest end is a closed socket.
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Proxies
// ---------------------------------------------------------------------------

/// A working SOCKS5 proxy on loopback. It connects every request to 127.0.0.1
/// at the requested port — so a test can ask for `files.test`, a name no
/// resolver knows, and prove the name travelled to the proxy rather than being
/// looked up here — and records what it was asked for and with which login.
pub struct FakeSocks5Proxy {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<(String, u16)>>>,
    pub logins: Arc<Mutex<Vec<String>>>,
    stop: CancellationToken,
}

impl FakeSocks5Proxy {
    pub async fn start(required_login: Option<(&str, &str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let logins = Arc::new(Mutex::new(Vec::new()));
        let stop = CancellationToken::new();
        let required = required_login.map(|(u, p)| (u.to_string(), p.to_string()));
        {
            let (requests, logins, stop) = (Arc::clone(&requests), Arc::clone(&logins), stop.clone());
            tokio::spawn(async move {
                loop {
                    let accepted = tokio::select! {
                        _ = stop.cancelled() => return,
                        accepted = listener.accept() => accepted,
                    };
                    let Ok((client, _)) = accepted else { continue };
                    let (requests, logins, required) = (Arc::clone(&requests), Arc::clone(&logins), required.clone());
                    tokio::spawn(async move {
                        let _ = socks5_session(client, requests, logins, required).await;
                    });
                }
            });
        }
        Self { port, requests, logins, stop }
    }
}

impl Drop for FakeSocks5Proxy {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn read_n(stream: &mut TcpStream, count: usize) -> std::io::Result<Vec<u8>> {
    let mut buffer = vec![0u8; count];
    stream.read_exact(&mut buffer).await?;
    Ok(buffer)
}

async fn socks5_session(
    mut s: TcpStream,
    requests: Arc<Mutex<Vec<(String, u16)>>>,
    logins: Arc<Mutex<Vec<String>>>,
    required: Option<(String, String)>,
) -> std::io::Result<()> {
    let head = read_n(&mut s, 2).await?;
    let methods = read_n(&mut s, head[1] as usize).await?;
    if let Some((user, password)) = required {
        if !methods.contains(&2) {
            s.write_all(&[5, 0xFF]).await?;
            return Ok(());
        }
        s.write_all(&[5, 2]).await?;
        read_n(&mut s, 1).await?;
        let user_length = read_n(&mut s, 1).await?[0] as usize;
        let given_user = String::from_utf8_lossy(&read_n(&mut s, user_length).await?).into_owned();
        let password_length = read_n(&mut s, 1).await?[0] as usize;
        let given_password = String::from_utf8_lossy(&read_n(&mut s, password_length).await?).into_owned();
        logins.lock().push(format!("{given_user}:{given_password}"));
        let ok = given_user == user && given_password == password;
        s.write_all(&[1, if ok { 0 } else { 1 }]).await?;
        if !ok {
            return Ok(());
        }
    } else {
        s.write_all(&[5, 0]).await?;
    }

    let request = read_n(&mut s, 4).await?;
    let host = match request[3] {
        1 => std::net::Ipv4Addr::from(<[u8; 4]>::try_from(read_n(&mut s, 4).await?).unwrap()).to_string(),
        4 => std::net::Ipv6Addr::from(<[u8; 16]>::try_from(read_n(&mut s, 16).await?).unwrap()).to_string(),
        _ => {
            let length = read_n(&mut s, 1).await?[0] as usize;
            String::from_utf8_lossy(&read_n(&mut s, length).await?).into_owned()
        }
    };
    let port_bytes = read_n(&mut s, 2).await?;
    let port = u16::from_be_bytes([port_bytes[0], port_bytes[1]]);
    requests.lock().push((host, port));

    let Ok(mut target) = TcpStream::connect(("127.0.0.1", port)).await else {
        s.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Ok(());
    };
    s.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).await?;
    let _ = tokio::io::copy_bidirectional(&mut s, &mut target).await;
    Ok(())
}

/// One connection, scripted: whatever arrives is read by the script, which
/// decides what goes back — the way a proxy's handshake is checked byte for byte.
pub struct ScriptedServer {
    pub port: u16,
    pub completion: tokio::task::JoinHandle<()>,
}

impl ScriptedServer {
    pub async fn start<F, Fut>(script: F) -> Self
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let completion = tokio::spawn(async move {
            let (client, _) = listener.accept().await.unwrap();
            script(client).await;
        });
        Self { port, completion }
    }

    pub async fn read(stream: &mut TcpStream, count: usize) -> Vec<u8> {
        read_n(stream, count).await.unwrap()
    }

    pub async fn read_head(stream: &mut TcpStream) -> String {
        read_head(stream).await.unwrap_or_default()
    }
}

/// An HTTP server that answers every request with the same small body.
pub async fn tiny_http_server(body: &'static str) -> TestServer {
    TestServer::start(move |_, _| Reply::ok(body.as_bytes()).header("Connection", "close")).await
}

// ---------------------------------------------------------------------------
// FTP
// ---------------------------------------------------------------------------

/// How the fake FTP server behaves.
#[derive(Clone)]
pub struct FtpBehaviour {
    pub login: Option<(String, String)>,
    pub feat_rest_stream: bool,
    pub honours_rest: bool,
    pub mdtm: Option<String>,
    /// Announce this (private) address in PASV, which the client must ignore.
    pub pasv_address: String,
}

impl Default for FtpBehaviour {
    fn default() -> Self {
        Self { login: None, feat_rest_stream: true, honours_rest: true, mdtm: Some("20260901083000".into()), pasv_address: "10,0,0,9".into() }
    }
}

/// Just enough of an FTP server to log in and hand over one file by passive
/// mode: the conversation a download has, with nothing else. Every control
/// connection is served; the commands each one saw are recorded.
pub struct FakeFtpServer {
    pub port: u16,
    pub commands: Arc<Mutex<Vec<String>>>,
    stop: CancellationToken,
}

impl FakeFtpServer {
    pub async fn start(file: Vec<u8>, behaviour: FtpBehaviour) -> Self {
        let control = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = control.local_addr().unwrap().port();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let stop = CancellationToken::new();
        let file = Arc::new(file);
        {
            let (commands, stop) = (Arc::clone(&commands), stop.clone());
            tokio::spawn(async move {
                loop {
                    let accepted = tokio::select! {
                        _ = stop.cancelled() => return,
                        accepted = control.accept() => accepted,
                    };
                    let Ok((client, _)) = accepted else { continue };
                    let (commands, file, behaviour) = (Arc::clone(&commands), Arc::clone(&file), behaviour.clone());
                    tokio::spawn(async move {
                        let _ = ftp_session(client, commands, file, behaviour).await;
                    });
                }
            });
        }
        Self { port, commands, stop }
    }

    pub fn url(&self, path: &str) -> String {
        format!("ftp://127.0.0.1:{}{path}", self.port)
    }

    pub fn commands(&self) -> Vec<String> {
        self.commands.lock().clone()
    }
}

impl Drop for FakeFtpServer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn ftp_session(stream: TcpStream, commands: Arc<Mutex<Vec<String>>>, file: Arc<Vec<u8>>, behaviour: FtpBehaviour) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);
    let data = TcpListener::bind("127.0.0.1:0").await?;
    let data_port = data.local_addr()?.port();
    let mut user = String::new();
    let mut offset = 0usize;

    write_half.write_all(b"220 ready\r\n").await?;
    loop {
        let mut line = String::new();
        if tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await? == 0 {
            return Ok(());
        }
        let command = line.trim_end().to_string();
        commands.lock().push(command.clone());
        let (verb, argument) = match command.split_once(' ') {
            Some((v, a)) => (v.to_ascii_uppercase(), a.to_string()),
            None => (command.to_ascii_uppercase(), String::new()),
        };
        let reply = match verb.as_str() {
            "USER" => {
                user = argument;
                "331 password please".to_string()
            }
            "PASS" => match &behaviour.login {
                Some((u, p)) if *u != user || *p != argument => "530 Login incorrect.".to_string(),
                _ => "230 logged in".to_string(),
            },
            "TYPE" => "200 binary".into(),
            "SIZE" => format!("213 {}", file.len()),
            "MDTM" => match &behaviour.mdtm {
                Some(stamp) => format!("213 {stamp}"),
                None => "550 no".into(),
            },
            "FEAT" => {
                if behaviour.feat_rest_stream {
                    "211-Features:\r\n SIZE\r\n REST STREAM\r\n211 End".into()
                } else {
                    "211-Features:\r\n SIZE\r\n211 End".into()
                }
            }
            "PASV" => format!("227 Entering Passive Mode ({},{},{})", behaviour.pasv_address, data_port >> 8, data_port & 0xFF),
            "REST" => {
                if behaviour.honours_rest {
                    offset = argument.parse().unwrap_or(0);
                    format!("350 Restarting at {offset}")
                } else {
                    "502 REST not implemented".into()
                }
            }
            "RETR" => {
                write_half.write_all(b"150 here it comes\r\n").await?;
                let (mut connection, _) = data.accept().await?;
                let start = offset.min(file.len());
                // A client that stops reading early closes this; that is fine.
                let _ = connection.write_all(&file[start..]).await;
                let _ = connection.shutdown().await;
                drop(connection);
                offset = 0;
                "226 done".into()
            }
            _ => "502 not here".into(),
        };
        write_half.write_all(format!("{reply}\r\n").as_bytes()).await?;
    }
}

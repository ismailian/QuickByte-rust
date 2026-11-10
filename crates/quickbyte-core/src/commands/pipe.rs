//! Port of `src/QuickByte.Core/Commands/CommandPipe.cs`.
//!
//! The channel between `qb.exe` and a running QuickByte: a named pipe, one JSON
//! request line in, one JSON response line out.
//!
//! A second pipe rather than more traffic on the single-instance one, which is
//! one-way by design — a launch hands over its command line and exits. A command
//! has an answer to wait for.
//!
//! Both ends are current-user-only (`platform::named_pipe`): the pipe is created
//! with an access list naming only this user, and the client checks the server
//! is owned by this user before it says anything. Another account on the machine
//! can neither send QuickByte a download nor impersonate QuickByte to `qb`.
//!
//! The messages are written in camelCase and read without regard to case, so
//! this build and a C# `qb.exe` (which wrote PascalCase) understand each other.

use std::future::Future;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::messages::{CommandRequest, CommandResponse};
use crate::error::{CoreError, Result};
use crate::platform::named_pipe;

/// A request is a handful of links and switches; anything this long is not one.
pub const MAX_MESSAGE_CHARS: usize = 64 * 1024;

/// Answered to a line that is not a request.
pub const NOT_UNDERSTOOD: &str = "QuickByte did not understand that request. Is qb the same version as QuickByte?";

/// How long one exchange may take before the server gives up on its client.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Per user, because pipe names are machine-wide: `QuickByte.Command.<UserName>`.
pub fn name() -> String {
    format!("QuickByte.Command.{}{}", named_pipe::current_user_name(), crate::helpers::app_paths::instance_suffix())
}

/// Sends `request` and waits for the answer. `Ok(None)` when nothing is
/// listening — QuickByte is not running — within `connect_timeout`, or when the
/// server hung up without one.
pub async fn send(request: &CommandRequest, connect_timeout: Duration, reply_timeout: Duration, pipe_name: Option<&str>) -> Result<Option<CommandResponse>> {
    let name = pipe_name.map_or_else(name, str::to_string);
    let Some(mut client) = named_pipe::connect(&name, connect_timeout).await? else { return Ok(None) };

    let exchange = async {
        write_line(&mut client, &serde_json::to_string(request)?).await?;
        match read_line(&mut client).await? {
            Some(line) => Ok(Some(serde_json::from_str::<CommandResponse>(&line)?)),
            None => Ok(None),
        }
    };
    tokio::time::timeout(reply_timeout, exchange).await.map_err(|_| CoreError::Timeout)?
}

/// Answers requests until `stopping` is cancelled, one client at a time —
/// commands run on the UI thread anyway, so a queue of them is what they would
/// be regardless. A malformed request is answered, not fatal.
pub async fn serve<F, Fut>(handle: F, stopping: CancellationToken, pipe_name: Option<&str>)
where
    F: Fn(CommandRequest) -> Fut,
    Fut: Future<Output = CommandResponse>,
{
    let name = pipe_name.map_or_else(name, str::to_string);
    while !stopping.is_cancelled() {
        let mut server = match named_pipe::create_server(&name) {
            Ok(server) => server,
            Err(_) => {
                // Another instance holds the name for a moment; try again shortly.
                tokio::select! {
                    _ = stopping.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(250)) => continue,
                }
            }
        };

        tokio::select! {
            _ = stopping.cancelled() => break,
            connected = server.connect() => if connected.is_err() { continue },
        }

        let exchange = async {
            let line = read_line(&mut server).await?;
            let request = line.as_deref().and_then(|l| serde_json::from_str::<CommandRequest>(l).ok());
            let response = match request {
                Some(request) => handle(request).await,
                None => CommandResponse::fail(NOT_UNDERSTOOD),
            };
            write_line(&mut server, &serde_json::to_string(&response)?).await
        };
        // A client that went away mid-exchange; the next one still gets an answer.
        tokio::select! {
            _ = stopping.cancelled() => break,
            _ = tokio::time::timeout(EXCHANGE_TIMEOUT, exchange) => {}
        }
    }
}

async fn write_line(stream: &mut (impl AsyncWrite + Unpin), json: &str) -> Result<()> {
    let mut bytes = Vec::with_capacity(json.len() + 1);
    bytes.extend_from_slice(json.as_bytes());
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// One line, read a byte at a time so nothing after it is taken, and never
/// longer than a request can be.
async fn read_line(stream: &mut (impl AsyncRead + Unpin)) -> Result<Option<String>> {
    let mut bytes = Vec::with_capacity(1024);
    let mut one = [0u8; 1];
    loop {
        let read = match stream.read(&mut one).await {
            Ok(n) => n,
            // A closed pipe is the end of the stream.
            Err(e) if e.raw_os_error() == Some(109) => 0,
            Err(e) => return Err(e.into()),
        };
        if read == 0 {
            return Ok((!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned()));
        }
        if one[0] == b'\n' {
            return Ok(Some(String::from_utf8_lossy(&bytes).into_owned()));
        }
        if bytes.len() >= MAX_MESSAGE_CHARS * 4 {
            return Err(CoreError::Io("The message is too long.".into()));
        }
        bytes.push(one[0]);
    }
}

#[cfg(all(test, windows))]
mod tests {
    //! The pipe parts of `CommandTests.cs`, each on a pipe name of its own so a
    //! running QuickByte is never reached.

    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;
    use crate::commands::messages::CommandVerb;

    fn unique() -> String {
        format!("QuickByte.Command.Test.{}", uuid::Uuid::new_v4().simple())
    }

    #[tokio::test]
    async fn a_request_is_answered_over_the_pipe() {
        let name = unique();
        let stop = CancellationToken::new();
        let received = Arc::new(Mutex::new(None));
        let serving = {
            let (name, stop, received) = (name.clone(), stop.clone(), Arc::clone(&received));
            tokio::spawn(async move {
                serve(
                    move |request: CommandRequest| {
                        let received = Arc::clone(&received);
                        async move {
                            let message = format!("got {}", request.urls.len());
                            *received.lock() = Some(request);
                            CommandResponse { ok: true, message: Some(message), ..Default::default() }
                        }
                    },
                    stop,
                    Some(&name),
                )
                .await
            })
        };

        let request = CommandRequest {
            verb: CommandVerb::Add,
            urls: vec!["https://a".into(), "https://b".into()],
            folder: Some(r"D:\Ünïcode".into()),
            ..Default::default()
        };
        let response = send(&request, Duration::from_secs(5), Duration::from_secs(5), Some(&name)).await.unwrap().unwrap();
        assert_eq!(response.message.as_deref(), Some("got 2"));
        assert_eq!(received.lock().as_ref().unwrap().folder.as_deref(), Some(r"D:\Ünïcode"));

        // And the next client is answered too.
        let status = CommandRequest::new(CommandVerb::Status);
        assert!(send(&status, Duration::from_secs(5), Duration::from_secs(5), Some(&name)).await.unwrap().is_some());

        stop.cancel();
        serving.await.unwrap();
    }

    #[tokio::test]
    async fn nobody_listening_is_an_answer_of_none_not_a_hang() {
        let started = std::time::Instant::now();
        let response = send(&CommandRequest::default(), Duration::from_millis(200), Duration::from_secs(5), Some(&unique())).await.unwrap();
        assert!(response.is_none());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn a_request_that_is_not_one_is_answered_with_an_error() {
        let name = unique();
        let stop = CancellationToken::new();
        let serving = {
            let (name, stop) = (name.clone(), stop.clone());
            tokio::spawn(async move {
                serve(|_: CommandRequest| async { panic!("must not be called") }, stop, Some(&name)).await;
            })
        };

        let mut client = named_pipe::connect(&name, Duration::from_secs(5)).await.unwrap().unwrap();
        client.write_all(b"{ not json\n").await.unwrap();
        let reply = read_line(&mut client).await.unwrap().unwrap();
        let response: CommandResponse = serde_json::from_str(&reply).unwrap();
        assert!(response.error.unwrap().contains("did not understand"));

        stop.cancel();
        serving.await.unwrap();
    }

    #[tokio::test]
    async fn a_c_sharp_qb_s_pascal_case_request_is_understood() {
        let name = unique();
        let stop = CancellationToken::new();
        let serving = {
            let (name, stop) = (name.clone(), stop.clone());
            tokio::spawn(async move {
                serve(
                    |request: CommandRequest| async move {
                        CommandResponse { ok: true, message: Some(request.target.unwrap_or_default()), ..Default::default() }
                    },
                    stop,
                    Some(&name),
                )
                .await;
            })
        };

        let mut client = named_pipe::connect(&name, Duration::from_secs(5)).await.unwrap().unwrap();
        client.write_all(b"{\"Verb\":3,\"Urls\":[],\"Target\":\"all\",\"Paused\":false}\n").await.unwrap();
        let reply: CommandResponse = serde_json::from_str(&read_line(&mut client).await.unwrap().unwrap()).unwrap();
        assert_eq!(reply.message.as_deref(), Some("all"));

        stop.cancel();
        serving.await.unwrap();
    }
}

//! One error type for the engine, replacing the C# exception types (see
//! MIGRATION.md §4.4). The messages are the C# messages, verbatim — they are
//! what the status bar, the dialogs and `qb` print.

use crate::helpers::byte_formatter::format_bytes;
use crate::helpers::disk_space::SpaceShortfall;

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

#[derive(Debug, Clone, thiserror::Error)]
pub enum CoreError {
    /// An HTTP `401` or FTP `530`. Never retried: a missing password fails
    /// identically forever, and the Add window must say what to do about it.
    #[error("{message}")]
    AuthenticationRequired { message: String, credentials_were_supplied: bool },

    /// The *proxy* wants a login, or refused the one in Options. Kept apart from
    /// `AuthenticationRequired` so a proxy password never goes to a site prompt.
    #[error("{0}")]
    ProxyAuthentication(String),

    /// The download will not fit on the disk it is writing to.
    #[error(
        "Not enough free space on {} — {} is needed and {} is free.",
        .0.volume.trim_end_matches('\\'),
        format_bytes(.0.needed),
        format_bytes(.0.available)
    )]
    InsufficientDiskSpace(SpaceShortfall),

    /// `If-Range` was answered with a whole body: the remote file changed.
    #[error("{0}")]
    RemoteFileChanged(String),

    /// A live or DRM-protected stream.
    #[error("{0}")]
    StreamNotSupported(String),

    /// The FTP server does not honour `REST`, so a partial chunk cannot continue.
    #[error("The FTP server cannot resume a transfer part way through.")]
    FtpRestartNotSupported,

    /// An HTTP status the caller did not expect.
    #[error("{message}")]
    Http { status: Option<u16>, message: String },

    #[error("{0}")]
    Io(String),

    #[error("The operation timed out.")]
    Timeout,

    #[error("The operation was cancelled.")]
    Cancelled,

    /// Bad input: a malformed URL, an unknown id, a refused request.
    #[error("{0}")]
    Invalid(String),

    #[error("{0}")]
    Other(String),
}

impl CoreError {
    pub const PROXY_AUTH_NEEDED: &'static str = "The proxy needs a user name and password. Enter them in Options > Proxy.";
    pub const PROXY_AUTH_REJECTED: &'static str = "The proxy rejected its user name or password. Check them in Options > Proxy.";

    pub fn auth(message: impl Into<String>, credentials_were_supplied: bool) -> Self {
        Self::AuthenticationRequired { message: message.into(), credentials_were_supplied }
    }
    pub fn http(status: Option<u16>, message: impl Into<String>) -> Self {
        Self::Http { status, message: message.into() }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    /// Whether `RetryPolicy` may spend an attempt on this. The non-transient
    /// kinds fail the same way however often they are asked.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::AuthenticationRequired { .. }
            | Self::ProxyAuthentication(_)
            | Self::InsufficientDiskSpace(_)
            | Self::RemoteFileChanged(_)
            | Self::StreamNotSupported(_)
            | Self::FtpRestartNotSupported
            | Self::Cancelled
            | Self::Invalid(_) => false,
            Self::Io(msg) => !is_disk_full_message(msg),
            Self::Http { .. } | Self::Timeout | Self::Other(_) => true,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

fn is_disk_full_message(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("not enough space") || m.contains("disk full") || m.contains("no space left")
}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::TimedOut => Self::Timeout,
            std::io::ErrorKind::Interrupted => Self::Cancelled,
            // ERROR_DISK_FULL (112) / ERROR_HANDLE_DISK_FULL (39): space does not
            // come back in a retry backoff, so this is not transient.
            _ if matches!(e.raw_os_error(), Some(112) | Some(39)) => Self::Io(format!("There is not enough space on the disk. ({e})")),
            _ => Self::Io(e.to_string()),
        }
    }
}

impl From<serde_json::Error> for CoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Other(e.to_string())
    }
}

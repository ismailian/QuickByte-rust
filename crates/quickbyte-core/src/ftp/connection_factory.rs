//! Port of `src/QuickByte.Core/Services/Ftp/FtpConnectionFactory.cs`.
//!
//! Produces [`FtpDownloadConnection`] workers.
//!
//! Stateless, and with no shared client to hold — unlike the HTTP factory, which
//! exists largely to keep one pooled client alive across every connection. FTP
//! control channels can't be pooled that way: each carries per-transfer state
//! (transfer type, restart offset, the passive port in flight), so each segment
//! opens and closes its own.

use std::sync::Arc;

use crate::ftp::download_connection::FtpDownloadConnection;
use crate::net::app_proxy::AppProxy;
use crate::traits::{ConnectionFactory, ConnectionSpec, DownloadConnection};

pub struct FtpConnectionFactory {
    proxy: Arc<AppProxy>,
}

impl Default for FtpConnectionFactory {
    fn default() -> Self {
        Self { proxy: Arc::clone(AppProxy::shared()) }
    }
}

impl FtpConnectionFactory {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_proxy(proxy: Arc<AppProxy>) -> Self {
        Self { proxy }
    }
}

impl ConnectionFactory for FtpConnectionFactory {
    fn create(&self, spec: ConnectionSpec) -> Arc<dyn DownloadConnection> {
        Arc::new(FtpDownloadConnection::with_proxy(spec, Arc::clone(&self.proxy)))
    }
}

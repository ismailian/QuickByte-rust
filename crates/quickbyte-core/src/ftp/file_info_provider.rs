//! Port of `src/QuickByte.Core/Services/Ftp/FtpFileInfoProvider.cs`.
//!
//! The `ftp://` half of the remote file info provider: logs in, asks `SIZE` for
//! the length and `FEAT` for restart support, and hands back the same
//! [`RemoteFileInfo`] shape the HTTP provider produces so nothing downstream has
//! to know which protocol answered.
//!
//! `supports_range_requests` means "the server will honour `REST`" here rather
//! than "it sends Accept-Ranges", but it decides the same thing either way:
//! whether the pool splits the file across connections or falls back to one.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::ftp::control_channel::FtpControlChannel;
use crate::ftp::ftp_url;
use crate::models::{RemoteFileInfo, RequestOptions};
use crate::net::app_proxy::AppProxy;
use crate::net::http_client::parse_url;

pub struct FtpFileInfoProvider {
    proxy: Arc<AppProxy>,
}

impl Default for FtpFileInfoProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FtpFileInfoProvider {
    pub fn new() -> Self {
        Self { proxy: Arc::clone(AppProxy::shared()) }
    }
    pub fn with_proxy(proxy: Arc<AppProxy>) -> Self {
        Self { proxy }
    }
}

#[async_trait]
impl crate::traits::RemoteFileInfoProvider for FtpFileInfoProvider {
    async fn get_file_info(&self, url: &str, options: &RequestOptions, cancel: &CancellationToken) -> Result<RemoteFileInfo> {
        let parsed = parse_url(url)?;
        let path = ftp_url::path_of(&parsed);

        let mut info = RemoteFileInfo {
            final_url: url.to_string(),
            file_name: ftp_url::file_name_of(&parsed),
            // FTP has no Content-Type. The category icons key off the extension
            // anyway, so a generic type here costs nothing.
            content_type: "application/octet-stream".into(),
            ..Default::default()
        };

        let mut channel = FtpControlChannel::connect(&parsed, options.credentials.as_ref(), cancel, Some(&self.proxy)).await?;
        info.content_length = channel.get_size(&path, cancel).await?;
        info.last_modified = channel.get_last_modified(&path, cancel).await?;

        // Restart is only worth asking about when the size is known: without a
        // length there is nothing to split, and the pool drops to one connection
        // regardless.
        info.supports_range_requests = info.has_known_size() && channel.supports_restart(cancel).await?;

        // FEAT is the server's own word for it, exactly like Accept-Ranges — and
        // unlike HTTP there is no cheap way to make it prove the claim, so the two
        // flags say the same thing here. The FTP connection is the safety net
        // instead: a refused REST discards the partial chunk and refetches from
        // zero rather than writing bytes at the wrong offset.
        info.server_claims_range_support = info.supports_range_requests;
        Ok(info)
    }
}

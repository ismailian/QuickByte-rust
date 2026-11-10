//! Port of `src/QuickByte.Core/Services/ProtocolFileInfoProvider.cs` and
//! `ProtocolConnectionFactory.cs`.
//!
//! The two dispatchers are the only things in the engine that look at a URL's
//! scheme, and they must stay in step: a file probed over one protocol and
//! fetched over the other resolves one size and downloads something else.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::ftp::connection_factory::FtpConnectionFactory;
use crate::ftp::file_info_provider::FtpFileInfoProvider;
use crate::ftp::ftp_url;
use crate::models::{RemoteFileInfo, RequestOptions};
use crate::net::download_connection::HttpConnectionFactory;
use crate::traits::{ConnectionFactory, ConnectionSpec, DownloadConnection, RemoteFileInfoProvider};

/// Picks the connection factory that matches a URL's scheme — FTP for `ftp://`
/// and `ftps://`, HTTP for everything else.
///
/// The dispatch lives here rather than inside the connection pool so the pool
/// keeps knowing nothing about protocols: it splits a range and runs N workers,
/// and whether those workers speak HTTP or FTP is settled before it ever sees
/// them. Adding a third protocol means adding a factory and one line here, not
/// touching the pool, the service, or the manager.
pub struct ProtocolConnectionFactory {
    http: Arc<dyn ConnectionFactory>,
    ftp: Arc<dyn ConnectionFactory>,
}

impl Default for ProtocolConnectionFactory {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl ProtocolConnectionFactory {
    pub fn new(http: Option<Arc<dyn ConnectionFactory>>, ftp: Option<Arc<dyn ConnectionFactory>>) -> Self {
        Self {
            http: http.unwrap_or_else(|| Arc::new(HttpConnectionFactory::new())),
            ftp: ftp.unwrap_or_else(|| Arc::new(FtpConnectionFactory::new())),
        }
    }
}

impl ConnectionFactory for ProtocolConnectionFactory {
    fn create(&self, spec: ConnectionSpec) -> Arc<dyn DownloadConnection> {
        let factory = if ftp_url::is_ftp(&spec.url) { &self.ftp } else { &self.http };
        factory.create(spec)
    }
}

/// Routes a metadata probe to the provider that speaks the URL's scheme, the
/// mirror image of [`ProtocolConnectionFactory`]. The two must agree: a file
/// probed over FTP and then fetched over HTTP would resolve one size and
/// download something else entirely.
pub struct ProtocolFileInfoProvider {
    http: Arc<dyn RemoteFileInfoProvider>,
    ftp: Arc<dyn RemoteFileInfoProvider>,
}

impl Default for ProtocolFileInfoProvider {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl ProtocolFileInfoProvider {
    pub fn new(http: Option<Arc<dyn RemoteFileInfoProvider>>, ftp: Option<Arc<dyn RemoteFileInfoProvider>>) -> Self {
        Self {
            http: http.unwrap_or_else(|| Arc::new(crate::net::remote_file_info_provider::RemoteFileInfoProvider::new())),
            ftp: ftp.unwrap_or_else(|| Arc::new(FtpFileInfoProvider::new())),
        }
    }
}

#[async_trait]
impl RemoteFileInfoProvider for ProtocolFileInfoProvider {
    async fn get_file_info(&self, url: &str, options: &RequestOptions, cancel: &CancellationToken) -> Result<RemoteFileInfo> {
        let provider = if ftp_url::is_ftp(url) { &self.ftp } else { &self.http };
        provider.get_file_info(url, options, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::*;
    use crate::enums::ConnectionStatus;
    use crate::models::{DownloadCredentials, DownloadSettings};

    const URLS: [(&str, bool); 7] = [
        ("ftp://example.com/disk.iso", true),
        ("FTP://example.com/disk.iso", true),
        ("ftps://example.com/disk.iso", true),
        ("FTPS://example.com/disk.iso", true),
        ("http://example.com/file.bin", false),
        ("https://example.com/file.bin", false),
        ("HTTPS://example.com/file.bin", false),
    ];

    struct Nothing;
    #[async_trait]
    impl DownloadConnection for Nothing {
        fn connection_id(&self) -> i32 {
            0
        }
        fn range_start(&self) -> i64 {
            0
        }
        fn range_end(&self) -> i64 {
            0
        }
        fn bytes_downloaded(&self) -> i64 {
            0
        }
        fn retry_count(&self) -> i32 {
            0
        }
        fn status(&self) -> ConnectionStatus {
            ConnectionStatus::Idle
        }
        fn last_error(&self) -> Option<String> {
            None
        }
        fn chunk_file_path(&self) -> &str {
            ""
        }
        async fn run(&self, _cancel: CancellationToken) -> Result<()> {
            Ok(())
        }
    }

    /// Everything a factory was handed: id, url, range, resume offset, chunk, options.
    type Seen = (i32, String, i64, i64, i64, String, RequestOptions);

    #[derive(Default)]
    struct RecordingConnectionFactory {
        seen: Mutex<Option<Seen>>,
    }
    impl RecordingConnectionFactory {
        fn called(&self) -> bool {
            self.seen.lock().is_some()
        }
    }
    impl ConnectionFactory for RecordingConnectionFactory {
        fn create(&self, spec: ConnectionSpec) -> Arc<dyn DownloadConnection> {
            *self.seen.lock() = Some((
                spec.connection_id,
                spec.url,
                spec.range_start,
                spec.range_end,
                spec.already_downloaded,
                spec.chunk_file_path,
                spec.options,
            ));
            Arc::new(Nothing)
        }
    }

    #[derive(Default)]
    struct RecordingInfoProvider {
        called: Mutex<bool>,
    }
    #[async_trait]
    impl RemoteFileInfoProvider for RecordingInfoProvider {
        async fn get_file_info(&self, _url: &str, _options: &RequestOptions, _cancel: &CancellationToken) -> Result<RemoteFileInfo> {
            *self.called.lock() = true;
            Ok(RemoteFileInfo::default())
        }
    }

    fn spec(url: &str) -> ConnectionSpec {
        ConnectionSpec {
            connection_id: 0,
            url: url.into(),
            range_start: 0,
            range_end: 99,
            already_downloaded: 0,
            chunk_file_path: "chunk.tmp".into(),
            settings: Arc::new(DownloadSettings::default()),
            bandwidth_limiter: None,
            options: RequestOptions::default(),
        }
    }

    #[test]
    fn the_connection_factory_routes_by_scheme() {
        for (url, expect_ftp) in URLS {
            let http = Arc::new(RecordingConnectionFactory::default());
            let ftp = Arc::new(RecordingConnectionFactory::default());
            ProtocolConnectionFactory::new(Some(http.clone()), Some(ftp.clone())).create(spec(url));
            assert_eq!(ftp.called(), expect_ftp, "{url}");
            assert_eq!(http.called(), !expect_ftp, "{url}");
        }
    }

    #[tokio::test]
    async fn the_file_info_provider_routes_by_the_same_rule() {
        for (url, expect_ftp) in URLS {
            let http = Arc::new(RecordingInfoProvider::default());
            let ftp = Arc::new(RecordingInfoProvider::default());
            ProtocolFileInfoProvider::new(Some(http.clone()), Some(ftp.clone()))
                .get_file_info(url, &RequestOptions::none(), &CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(*ftp.called.lock(), expect_ftp, "{url}");
            assert_eq!(*http.called.lock(), !expect_ftp, "{url}");
        }
    }

    #[tokio::test]
    async fn both_dispatchers_agree_about_every_url() {
        // The invariant stated as one assertion: whichever protocol answers the
        // probe is the one that fetches the bytes.
        for (url, expect_ftp) in URLS {
            let probe_http = Arc::new(RecordingInfoProvider::default());
            let probe_ftp = Arc::new(RecordingInfoProvider::default());
            let fetch_http = Arc::new(RecordingConnectionFactory::default());
            let fetch_ftp = Arc::new(RecordingConnectionFactory::default());

            ProtocolFileInfoProvider::new(Some(probe_http.clone()), Some(probe_ftp.clone()))
                .get_file_info(url, &RequestOptions::none(), &CancellationToken::new())
                .await
                .unwrap();
            ProtocolConnectionFactory::new(Some(fetch_http.clone()), Some(fetch_ftp.clone())).create(spec(url));

            assert_eq!(*probe_ftp.called.lock(), fetch_ftp.called());
            assert_eq!(*probe_http.called.lock(), fetch_http.called());
            assert_eq!(*probe_ftp.called.lock(), expect_ftp);
        }
    }

    #[test]
    fn something_that_is_not_a_url_falls_to_http() {
        // It cannot be parsed, so it is not FTP — and HTTP will produce the error
        // the user needs to see.
        let http = Arc::new(RecordingConnectionFactory::default());
        let ftp = Arc::new(RecordingConnectionFactory::default());
        ProtocolConnectionFactory::new(Some(http.clone()), Some(ftp.clone())).create(spec("not a url"));
        assert!(http.called());
        assert!(!ftp.called());
    }

    #[test]
    fn the_dispatcher_passes_every_argument_through_untouched() {
        let ftp = Arc::new(RecordingConnectionFactory::default());
        let options = RequestOptions { credentials: Some(DownloadCredentials::new("alice", "")), ..Default::default() };
        let mut s = spec("ftp://example.com/x.iso");
        s.connection_id = 3;
        s.range_start = 100;
        s.range_end = 199;
        s.already_downloaded = 40;
        s.chunk_file_path = "part3.tmp".into();
        s.options = options.clone();

        ProtocolConnectionFactory::new(Some(Arc::new(RecordingConnectionFactory::default())), Some(ftp.clone())).create(s);

        let seen = ftp.seen.lock().clone().unwrap();
        assert_eq!(seen, (3, "ftp://example.com/x.iso".to_string(), 100, 199, 40, "part3.tmp".to_string(), options));
    }
}

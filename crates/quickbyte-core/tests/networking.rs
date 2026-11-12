//! The networking half of the C# `QuickByte.Core.Tests` suite, against local
//! servers: the HTTP connection, the probe, sign-in handshakes, the proxy and
//! its tunnels, FTP, the update check, the browser bridge and playlist fetching.

mod support;

#[path = "networking/authenticated_http.rs"]
mod authenticated_http;
#[path = "networking/browser_integration.rs"]
mod browser_integration;
#[path = "networking/download_connection.rs"]
mod download_connection;
#[path = "networking/ftp.rs"]
mod ftp;
#[path = "networking/proxy.rs"]
mod proxy;
#[path = "networking/remote_file_info_provider.rs"]
mod remote_file_info_provider;
#[path = "networking/streaming.rs"]
mod streaming;
#[path = "networking/update_service.rs"]
mod update_service;
#[path = "networking/virus_total.rs"]
mod virus_total;

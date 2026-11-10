//! HTTP: the shared client, the proxy, authentication, probing and the HTTP connection.

pub mod app_proxy;
pub mod authenticated_http;
pub mod digest;
pub mod download_connection;
pub mod http_client;
pub mod proxy_bypass;
pub mod proxy_tunnel;
pub mod remote_file_info_provider;
pub mod request_decorator;
pub mod segment_range;
pub mod sspi;

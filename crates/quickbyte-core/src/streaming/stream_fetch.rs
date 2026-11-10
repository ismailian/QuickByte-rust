//! Port of `src/QuickByte.Core/Streaming/StreamFetch.cs`.
//!
//! Reads a playlist or manifest off the web, the way the pieces it lists will be
//! read: with the download's cookies, referrer and login, since a player on a
//! signed-in page is typically the only thing its playlist would answer.

use reqwest::StatusCode;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::{CoreError, Result};
use crate::models::RequestOptions;
use crate::net::http_client::{HttpClient, HttpRequest, HttpResponse};
use crate::net::{authenticated_http, request_decorator};

/// A playlist for a ten-hour video is a few megabytes. Past this it is not a playlist.
pub const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;

/// Pieces, keys and playlists must be on the web: one pointing at `file:` would
/// have this read the user's own disk.
pub fn is_web(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

/// The text, and the address it finally came from — relative piece addresses
/// resolve against that, not the one asked for.
pub async fn get_text(http: &HttpClient, url: &Url, options: Option<&RequestOptions>, cancel: &CancellationToken) -> Result<(String, Url)> {
    if !is_web(url) {
        return Err(CoreError::invalid("A stream's playlist has to be on the web (http or https)."));
    }

    let mut request = HttpRequest::get(url.clone());
    request_decorator::apply(&mut request.headers, options);
    let response = authenticated_http::send(http, &request, options, cancel).await?;
    let response = ensure_success(response, options)?;

    if response.content_length().is_some_and(|l| l > MAX_DOCUMENT_BYTES as i64) {
        return Err(CoreError::invalid("This is too large to be a playlist."));
    }
    let final_url = response.url().clone();
    let bytes = response.read_at_most(MAX_DOCUMENT_BYTES).await?.ok_or_else(|| CoreError::invalid("This is too large to be a playlist."))?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), final_url))
}

/// The one place a stream's HTTP answer is judged, so a playlist, a piece and a
/// key report a refused login identically — as the error the Add window turns
/// into a sign-in prompt, and which is never retried.
pub fn ensure_success(response: HttpResponse, options: Option<&RequestOptions>) -> Result<HttpResponse> {
    if response.status() == StatusCode::PROXY_AUTHENTICATION_REQUIRED {
        return Err(CoreError::ProxyAuthentication(CoreError::PROXY_AUTH_NEEDED.into()));
    }
    if response.status() == StatusCode::UNAUTHORIZED {
        let supplied = options.is_some_and(RequestOptions::has_credentials);
        return Err(CoreError::auth(
            if supplied { "The server rejected the supplied user name or password." } else { "The server requires a user name and password." },
            supplied,
        ));
    }
    response.error_for_status()
}

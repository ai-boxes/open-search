use bytes::{Bytes, BytesMut};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum BoundedBodyError {
    #[error("failed to read response body")]
    Read,
    #[error("response body was too large")]
    TooLarge,
}

pub(crate) async fn collect_bounded_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Bytes, BoundedBodyError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(BoundedBodyError::TooLarge);
    }

    let mut body = BytesMut::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| BoundedBodyError::Read)? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(BoundedBodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

pub(crate) fn validate_oauth_endpoint(
    endpoint: &str,
    field: &str,
    allow_insecure_localhost: bool,
) -> Result<(), String> {
    let endpoint =
        reqwest::Url::parse(endpoint).map_err(|_| format!("Grok OAuth {field} is invalid"))?;
    let host = endpoint.host_str().unwrap_or_default().to_ascii_lowercase();
    let secure_xai = endpoint.scheme() == "https" && (host == "x.ai" || host.ends_with(".x.ai"));
    let local_test = allow_insecure_localhost
        && endpoint.scheme() == "http"
        && matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1");
    if !secure_xai && !local_test {
        return Err(format!("Grok OAuth {field} must use HTTPS on an x.ai host"));
    }
    Ok(())
}

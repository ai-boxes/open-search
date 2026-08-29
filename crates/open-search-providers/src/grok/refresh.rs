use std::time::{Duration, SystemTime, UNIX_EPOCH};

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use thiserror::Error;

use super::{
    contract::{OIDC_CLIENT_ID, OIDC_ISSUER},
    credentials::{GrokAuthError, GrokCredentials},
    http::{BoundedBodyError, collect_bounded_body, validate_oauth_endpoint},
};

const MAX_RESPONSE_SIZE: usize = 64 * 1024;

#[derive(Clone)]
pub struct GrokRefreshClient {
    http: reqwest::Client,
    discovery_url_override: Option<String>,
    allow_insecure_localhost: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct RefreshedGrokTokens {
    pub(crate) access_token: SecretString,
    pub(crate) refresh_token: Option<SecretString>,
    pub(crate) id_token: Option<SecretString>,
    pub(crate) token_type: Option<String>,
    pub(crate) expires_in: u32,
    pub(crate) token_endpoint: String,
}

impl GrokRefreshClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            discovery_url_override: None,
            allow_insecure_localhost: false,
        }
    }

    pub async fn refresh(
        &self,
        credentials: &GrokCredentials,
    ) -> Result<GrokCredentials, GrokRefreshError> {
        let refresh_token = credentials
            .refresh_token()
            .ok_or_else(|| GrokRefreshError::reauth("Grok credential is missing refresh_token"))?;
        let issuer = credentials.oidc_issuer().ok_or_else(|| {
            GrokRefreshError::reauth("Grok credential is missing oidc_issuer provenance")
        })?;
        let client_id = credentials.oidc_client_id().ok_or_else(|| {
            GrokRefreshError::reauth("Grok credential is missing oidc_client_id provenance")
        })?;
        if issuer.trim_end_matches('/') != OIDC_ISSUER || client_id != OIDC_CLIENT_ID {
            return Err(GrokRefreshError::reauth(
                "Grok credential has an unsupported OAuth provenance",
            ));
        }
        validate_oauth_endpoint(issuer, "issuer", self.allow_insecure_localhost)
            .map_err(GrokRefreshError::internal)?;
        let token_endpoint = self.discover_token_endpoint(issuer).await?;

        let response = self
            .http
            .post(&token_endpoint)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", client_id),
                ("refresh_token", refresh_token.expose_secret()),
            ])
            .send()
            .await
            .map_err(|_| GrokRefreshError::transient("Grok token refresh request failed"))?;
        let status = response.status();
        let body = collect_bounded_body(response, MAX_RESPONSE_SIZE)
            .await
            .map_err(|error| body_error(error, "Grok token refresh"))?;

        if !status.is_success() {
            let error_code = serde_json::from_slice::<OAuthErrorResponse>(&body)
                .ok()
                .and_then(|response| response.error);
            let kind = match error_code.as_deref() {
                Some("invalid_client" | "invalid_grant" | "invalid_token") => {
                    GrokRefreshErrorKind::ReauthorizationRequired
                }
                _ if status.as_u16() == 401 => GrokRefreshErrorKind::ReauthorizationRequired,
                _ if status.as_u16() == 429 || status.is_server_error() => {
                    GrokRefreshErrorKind::Transient
                }
                _ => GrokRefreshErrorKind::Internal,
            };
            return Err(GrokRefreshError::new(
                kind,
                format!("Grok token refresh returned HTTP {status}"),
            ));
        }

        let response: TokenResponse = serde_json::from_slice(&body)
            .map_err(|_| GrokRefreshError::transient("Grok token refresh returned invalid JSON"))?;
        let access_token = non_empty_secret(response.access_token).ok_or_else(|| {
            GrokRefreshError::transient("Grok token refresh response is missing access_token")
        })?;
        let expires_in = u32::try_from(response.expires_in)
            .ok()
            .filter(|expires_in| *expires_in > 0)
            .ok_or_else(|| {
                GrokRefreshError::transient("Grok token refresh response has invalid expires_in")
            })?;
        let tokens = RefreshedGrokTokens {
            access_token,
            refresh_token: non_empty_secret(response.refresh_token),
            id_token: non_empty_secret(response.id_token),
            token_type: non_empty_string(response.token_type),
            expires_in,
            token_endpoint,
        };
        credentials
            .refreshed(&tokens, unix_timestamp())
            .map_err(GrokRefreshError::credential)
    }

    async fn discover_token_endpoint(&self, issuer: &str) -> Result<String, GrokRefreshError> {
        let discovery_url = self.discovery_url_override.clone().unwrap_or_else(|| {
            format!(
                "{}/.well-known/openid-configuration",
                issuer.trim_end_matches('/')
            )
        });
        let response = self
            .http
            .get(discovery_url)
            .send()
            .await
            .map_err(|_| GrokRefreshError::transient("Grok OIDC discovery request failed"))?;
        let status = response.status();
        let body = collect_bounded_body(response, MAX_RESPONSE_SIZE)
            .await
            .map_err(|error| body_error(error, "Grok OIDC discovery"))?;
        if !status.is_success() {
            return Err(GrokRefreshError::transient(format!(
                "Grok OIDC discovery returned HTTP {status}"
            )));
        }
        let discovery: DiscoveryResponse = serde_json::from_slice(&body).map_err(|_| {
            GrokRefreshError::transient("Grok OIDC discovery returned invalid JSON")
        })?;
        if discovery.issuer.trim_end_matches('/') != issuer.trim_end_matches('/') {
            return Err(GrokRefreshError::transient(
                "Grok OIDC discovery issuer did not match credential provenance",
            ));
        }
        let token_endpoint = discovery.token_endpoint.trim();
        if token_endpoint.is_empty() {
            return Err(GrokRefreshError::transient(
                "Grok OIDC discovery is missing token_endpoint",
            ));
        }
        validate_oauth_endpoint(
            token_endpoint,
            "token_endpoint",
            self.allow_insecure_localhost,
        )
        .map_err(GrokRefreshError::internal)?;
        Ok(token_endpoint.to_owned())
    }

    #[cfg(test)]
    pub(crate) fn for_test(discovery_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            discovery_url_override: Some(discovery_url.into()),
            allow_insecure_localhost: true,
        }
    }
}

impl Default for GrokRefreshClient {
    fn default() -> Self {
        Self::new()
    }
}

fn body_error(error: BoundedBodyError, operation: &str) -> GrokRefreshError {
    match error {
        BoundedBodyError::Read => {
            GrokRefreshError::transient(format!("failed to read {operation} response"))
        }
        BoundedBodyError::TooLarge => {
            GrokRefreshError::transient(format!("{operation} response was too large"))
        }
    }
}

fn non_empty_secret(value: Option<String>) -> Option<SecretString> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(SecretString::from)
}

fn non_empty_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct OAuthErrorResponse {
    error: Option<String>,
}

#[derive(Deserialize)]
struct DiscoveryResponse {
    issuer: String,
    token_endpoint: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: Option<String>,
    #[serde(default)]
    expires_in: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrokRefreshErrorKind {
    ReauthorizationRequired,
    Transient,
    Internal,
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct GrokRefreshError {
    kind: GrokRefreshErrorKind,
    message: String,
}

impl GrokRefreshError {
    fn new(kind: GrokRefreshErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn reauth(message: impl Into<String>) -> Self {
        Self::new(GrokRefreshErrorKind::ReauthorizationRequired, message)
    }

    fn transient(message: impl Into<String>) -> Self {
        Self::new(GrokRefreshErrorKind::Transient, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(GrokRefreshErrorKind::Internal, message)
    }

    fn credential(error: GrokAuthError) -> Self {
        Self::internal(error.to_string())
    }

    pub const fn kind(&self) -> GrokRefreshErrorKind {
        self.kind
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Router,
        extract::State,
        http::StatusCode,
        routing::{get, post},
    };

    use super::*;

    #[tokio::test]
    async fn sends_refresh_form_and_keeps_rotated_tokens() {
        let captured = Arc::new(Mutex::new(String::new()));
        let base_url = Arc::new(Mutex::new(String::new()));
        let app = Router::new()
            .route(
                "/discovery",
                get({
                    let base_url = base_url.clone();
                    move || {
                        let base_url = base_url.clone();
                        async move {
                            let base_url = base_url.lock().expect("base URL lock").clone();
                            axum::Json(serde_json::json!({
                                "issuer": OIDC_ISSUER,
                                "token_endpoint": format!("{base_url}/token")
                            }))
                        }
                    }
                }),
            )
            .route(
                "/token",
                post(
                    |State(captured): State<Arc<Mutex<String>>>, body: String| async move {
                        *captured.lock().expect("capture lock") = body;
                        r#"{"access_token":"new-access","refresh_token":"new-refresh","token_type":"Bearer","expires_in":3600}"#
                    },
                ),
            )
            .with_state(captured.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind refresh endpoint");
        let address = listener.local_addr().expect("refresh endpoint address");
        let local_base_url = format!("http://{address}");
        *base_url.lock().expect("base URL lock") = local_base_url.clone();
        let server = tokio::spawn(axum::serve(listener, app).into_future());
        let credentials = GrokCredentials::from_json(&SecretString::from(format!(
            r#"{{"type":"xai","auth_kind":"oauth","access_token":"old-access","refresh_token":"old refresh","oidc_issuer":"{OIDC_ISSUER}","oidc_client_id":"{OIDC_CLIENT_ID}"}}"#
        )))
        .expect("credentials");

        let refreshed = GrokRefreshClient::for_test(format!("{local_base_url}/discovery"))
            .refresh(&credentials)
            .await
            .expect("refreshed credential");
        server.abort();

        let document: serde_json::Value = serde_json::from_str(
            refreshed
                .to_json()
                .expect("credential JSON")
                .expose_secret(),
        )
        .expect("credential document");
        assert_eq!(document["access_token"], "new-access");
        assert_eq!(document["refresh_token"], "new-refresh");
        let body = captured.lock().expect("captured form");
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=old+refresh"));
    }

    #[tokio::test]
    async fn invalid_grant_requires_reauthorization_without_echoing_body() {
        let base_url = Arc::new(Mutex::new(String::new()));
        let app = Router::new()
            .route(
                "/discovery",
                get({
                    let base_url = base_url.clone();
                    move || {
                        let base_url = base_url.clone();
                        async move {
                            let base_url = base_url.lock().expect("base URL lock").clone();
                            axum::Json(serde_json::json!({
                                "issuer": OIDC_ISSUER,
                                "token_endpoint": format!("{base_url}/token")
                            }))
                        }
                    }
                }),
            )
            .route(
                "/token",
                post(|| async {
                    (
                        StatusCode::BAD_REQUEST,
                        r#"{"error":"invalid_grant","error_description":"do-not-log"}"#,
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind refresh endpoint");
        let address = listener.local_addr().expect("refresh endpoint address");
        let local_base_url = format!("http://{address}");
        *base_url.lock().expect("base URL lock") = local_base_url.clone();
        let server = tokio::spawn(axum::serve(listener, app).into_future());
        let credentials = GrokCredentials::from_json(&SecretString::from(format!(
            r#"{{"type":"xai","auth_kind":"oauth","access_token":"old-access","refresh_token":"old-refresh","oidc_issuer":"{OIDC_ISSUER}","oidc_client_id":"{OIDC_CLIENT_ID}"}}"#
        )))
        .expect("credentials");

        let error = GrokRefreshClient::for_test(format!("{local_base_url}/discovery"))
            .refresh(&credentials)
            .await
            .expect_err("invalid grant should fail");
        server.abort();

        assert_eq!(error.kind(), GrokRefreshErrorKind::ReauthorizationRequired);
        assert!(!error.to_string().contains("do-not-log"));
        assert!(!error.to_string().contains("old-refresh"));
    }

    #[tokio::test]
    async fn server_failure_is_transient_without_echoing_body() {
        let error = refresh_error_for_response(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error_description":"do-not-log"}"#,
        )
        .await;

        assert_eq!(error.kind(), GrokRefreshErrorKind::Transient);
        assert!(!error.to_string().contains("do-not-log"));
    }

    async fn refresh_error_for_response(
        status: StatusCode,
        response_body: &'static str,
    ) -> GrokRefreshError {
        let base_url = Arc::new(Mutex::new(String::new()));
        let app = Router::new()
            .route(
                "/discovery",
                get({
                    let base_url = base_url.clone();
                    move || {
                        let base_url = base_url.clone();
                        async move {
                            let base_url = base_url.lock().expect("base URL lock").clone();
                            axum::Json(serde_json::json!({
                                "issuer": OIDC_ISSUER,
                                "token_endpoint": format!("{base_url}/token")
                            }))
                        }
                    }
                }),
            )
            .route(
                "/token",
                post(move || async move { (status, response_body) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind refresh endpoint");
        let address = listener.local_addr().expect("refresh endpoint address");
        let local_base_url = format!("http://{address}");
        *base_url.lock().expect("base URL lock") = local_base_url.clone();
        let server = tokio::spawn(axum::serve(listener, app).into_future());
        let credentials = GrokCredentials::from_json(&SecretString::from(format!(
            r#"{{"type":"xai","auth_kind":"oauth","access_token":"old-access","refresh_token":"old-refresh","oidc_issuer":"{OIDC_ISSUER}","oidc_client_id":"{OIDC_CLIENT_ID}"}}"#
        )))
        .expect("credentials");

        let error = GrokRefreshClient::for_test(format!("{local_base_url}/discovery"))
            .refresh(&credentials)
            .await
            .expect_err("refresh should fail");
        server.abort();
        error
    }
}

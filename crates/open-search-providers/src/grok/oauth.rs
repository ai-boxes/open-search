use std::time::{Duration, SystemTime, UNIX_EPOCH};

use secrecy::SecretString;
use serde::Deserialize;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{
    contract::{DISCOVERY_URL, OIDC_CLIENT_ID, OIDC_ISSUER, PROXY_BASE_URL},
    credentials::{GrokAuthError, GrokCredentials},
    http::{BoundedBodyError, collect_bounded_body, validate_oauth_endpoint},
    identity::session_headers,
};

const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_RESPONSE_SIZE: usize = 64 * 1024;

#[derive(Clone)]
pub struct GrokOAuthClient {
    http: reqwest::Client,
    discovery_url: String,
    proxy_base_url: String,
    allow_insecure_localhost: bool,
    minimum_poll_interval: Duration,
}

pub struct StartedGrokOAuth {
    pub challenge: GrokOAuthChallenge,
    pub pending: PendingGrokOAuth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokOAuthChallenge {
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    pub expires_at: i64,
    pub interval_seconds: u64,
}

pub struct PendingGrokOAuth {
    http: reqwest::Client,
    token_endpoint: String,
    oidc_issuer: String,
    proxy_base_url: String,
    device_code: String,
    interval: Duration,
    interval_increment: Duration,
    expires_at: i64,
}

impl GrokOAuthClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            discovery_url: DISCOVERY_URL.to_owned(),
            proxy_base_url: PROXY_BASE_URL.to_owned(),
            allow_insecure_localhost: false,
            minimum_poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }

    pub async fn start(&self) -> Result<StartedGrokOAuth, GrokOAuthError> {
        let discovery: DiscoveryResponse = self
            .get_json(&self.discovery_url, "Grok OAuth discovery")
            .await?;
        validate_oauth_endpoint(&discovery.issuer, "issuer", self.allow_insecure_localhost)
            .map_err(GrokOAuthError::InvalidResponse)?;
        if discovery.issuer.trim_end_matches('/') != OIDC_ISSUER {
            return Err(GrokOAuthError::InvalidResponse(
                "Grok OAuth discovery returned an unsupported issuer".to_owned(),
            ));
        }
        validate_oauth_endpoint(
            &discovery.device_authorization_endpoint,
            "device_authorization_endpoint",
            self.allow_insecure_localhost,
        )
        .map_err(GrokOAuthError::InvalidResponse)?;
        validate_oauth_endpoint(
            &discovery.token_endpoint,
            "token_endpoint",
            self.allow_insecure_localhost,
        )
        .map_err(GrokOAuthError::InvalidResponse)?;

        let response = self
            .http
            .post(&discovery.device_authorization_endpoint)
            .form(&[("client_id", OIDC_CLIENT_ID), ("scope", SCOPE)])
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| GrokOAuthError::Request("Grok device authorization request failed"))?;
        let response: DeviceCodeResponse =
            response_json(response, "Grok device authorization").await?;
        let device_code = required(response.device_code, "device_code")?;
        let user_code = required(response.user_code, "user_code")?;
        let verification_uri = response
            .verification_uri
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                response
                    .verification_uri_complete
                    .as_deref()
                    .and_then(verification_base_uri)
            })
            .ok_or_else(|| {
                GrokOAuthError::InvalidResponse(
                    "Grok device authorization response is missing verification_uri".to_owned(),
                )
            })?;
        let verification_uri_complete = response
            .verification_uri_complete
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let expires_in = u64::try_from(response.expires_in)
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                GrokOAuthError::InvalidResponse(
                    "Grok device authorization response has invalid expires_in".to_owned(),
                )
            })?;
        let requested_interval = u64::try_from(response.interval)
            .ok()
            .filter(|value| *value > 0)
            .map(Duration::from_secs)
            .unwrap_or(self.minimum_poll_interval);
        let interval = requested_interval.max(self.minimum_poll_interval);
        let expires_at = unix_timestamp()
            .checked_add(i64::try_from(expires_in).map_err(|_| {
                GrokOAuthError::InvalidResponse(
                    "Grok device authorization expiry is too large".to_owned(),
                )
            })?)
            .ok_or_else(|| {
                GrokOAuthError::InvalidResponse(
                    "Grok device authorization expiry is too large".to_owned(),
                )
            })?;

        Ok(StartedGrokOAuth {
            challenge: GrokOAuthChallenge {
                verification_uri,
                verification_uri_complete,
                user_code,
                expires_at,
                interval_seconds: interval.as_secs(),
            },
            pending: PendingGrokOAuth {
                http: self.http.clone(),
                token_endpoint: discovery.token_endpoint,
                oidc_issuer: discovery.issuer,
                proxy_base_url: self.proxy_base_url.clone(),
                device_code,
                interval,
                interval_increment: self.minimum_poll_interval,
                expires_at,
            },
        })
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        operation: &str,
    ) -> Result<T, GrokOAuthError> {
        let response = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| GrokOAuthError::Request("Grok OAuth discovery request failed"))?;
        response_json(response, operation).await
    }

    #[cfg(test)]
    fn for_test(discovery_url: impl Into<String>, proxy_base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            discovery_url: discovery_url.into(),
            proxy_base_url: proxy_base_url.into(),
            allow_insecure_localhost: true,
            minimum_poll_interval: Duration::from_millis(1),
        }
    }
}

impl Default for GrokOAuthClient {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingGrokOAuth {
    pub async fn complete(self) -> Result<GrokCredentials, GrokOAuthError> {
        let mut interval = self.interval;
        let mut first_attempt = true;
        loop {
            if unix_timestamp() >= self.expires_at {
                return Err(GrokOAuthError::Expired);
            }
            if !first_attempt {
                tokio::time::sleep(interval).await;
            }
            first_attempt = false;

            let response = self
                .http
                .post(&self.token_endpoint)
                .form(&[
                    ("grant_type", DEVICE_GRANT_TYPE),
                    ("device_code", self.device_code.as_str()),
                    ("client_id", OIDC_CLIENT_ID),
                ])
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(|_| GrokOAuthError::Request("Grok device token request failed"))?;
            let status = response.status();
            let body = collect_bounded_body(response, MAX_RESPONSE_SIZE)
                .await
                .map_err(|error| body_error(error, "Grok device token"))?;
            let token: DeviceTokenResponse = serde_json::from_slice(&body).map_err(|_| {
                GrokOAuthError::InvalidResponse(
                    "Grok device token response returned invalid JSON".to_owned(),
                )
            })?;
            if let Some(error) = token.error.as_deref() {
                match error {
                    "authorization_pending" => continue,
                    "slow_down" => {
                        interval = interval.saturating_add(self.interval_increment);
                        continue;
                    }
                    "expired_token" => return Err(GrokOAuthError::Expired),
                    "access_denied" => return Err(GrokOAuthError::Denied),
                    _ => return Err(GrokOAuthError::OAuth),
                }
            }
            if !status.is_success() {
                return Err(GrokOAuthError::InvalidResponse(format!(
                    "Grok device token request returned HTTP {status}"
                )));
            }

            let access_token = required(token.access_token, "access_token")?;
            let refresh_token = required(token.refresh_token, "refresh_token")?;
            let upstream_user_id =
                fetch_user_id(&self.http, &self.proxy_base_url, access_token.as_str()).await?;
            let refreshed_at = unix_timestamp();
            let expires_in = u64::try_from(token.expires_in)
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    GrokOAuthError::InvalidResponse(
                        "Grok device token response has invalid expires_in".to_owned(),
                    )
                })?;
            let expired_at = refreshed_at
                .checked_add(i64::try_from(expires_in).map_err(|_| {
                    GrokOAuthError::InvalidResponse("Grok token expiry is too large".to_owned())
                })?)
                .ok_or_else(|| {
                    GrokOAuthError::InvalidResponse("Grok token expiry is too large".to_owned())
                })?;
            let document = serde_json::json!({
                "type": "xai",
                "auth_kind": "oauth",
                "access_token": access_token,
                "refresh_token": refresh_token,
                "upstream_user_id": upstream_user_id,
                "id_token": token.id_token.filter(|value| !value.trim().is_empty()),
                "token_type": token.token_type.filter(|value| !value.trim().is_empty()),
                "expires_in": expires_in,
                "expired": timestamp_rfc3339(expired_at)?,
                "last_refresh": timestamp_rfc3339(refreshed_at)?,
                "token_endpoint": self.token_endpoint,
                "oidc_issuer": self.oidc_issuer,
                "oidc_client_id": OIDC_CLIENT_ID,
                "disabled": false
            });
            let credential_json = serde_json::to_string(&document).map_err(|_| {
                GrokOAuthError::InvalidResponse(
                    "failed to serialize Grok OAuth credential".to_owned(),
                )
            })?;
            return GrokCredentials::from_json(&SecretString::from(credential_json))
                .map_err(GrokOAuthError::Credential);
        }
    }
}

async fn fetch_user_id(
    http: &reqwest::Client,
    proxy_base_url: &str,
    access_token: &str,
) -> Result<String, GrokOAuthError> {
    let response = session_headers(
        http.get(format!("{}/user", proxy_base_url.trim_end_matches('/')))
            .bearer_auth(access_token),
    )
    .timeout(Duration::from_secs(10))
    .send()
    .await
    .map_err(|_| GrokOAuthError::Request("Grok user request failed"))?;
    let response: UserResponse = response_json(response, "Grok user").await?;
    let user_id = response.user_id.trim();
    if user_id.is_empty() {
        return Err(GrokOAuthError::InvalidResponse(
            "Grok user response is missing userId".to_owned(),
        ));
    }
    Ok(user_id.to_owned())
}

async fn response_json<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T, GrokOAuthError> {
    let status = response.status();
    let body = collect_bounded_body(response, MAX_RESPONSE_SIZE)
        .await
        .map_err(|error| body_error(error, operation))?;
    if !status.is_success() {
        return Err(GrokOAuthError::InvalidResponse(format!(
            "{operation} returned HTTP {status}"
        )));
    }
    serde_json::from_slice(&body)
        .map_err(|_| GrokOAuthError::InvalidResponse(format!("{operation} returned invalid JSON")))
}

fn body_error(error: BoundedBodyError, operation: &str) -> GrokOAuthError {
    match error {
        BoundedBodyError::Read => {
            GrokOAuthError::InvalidResponse(format!("failed to read {operation} response"))
        }
        BoundedBodyError::TooLarge => {
            GrokOAuthError::InvalidResponse(format!("{operation} response was too large"))
        }
    }
}

fn required(value: Option<String>, field: &str) -> Result<String, GrokOAuthError> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            GrokOAuthError::InvalidResponse(format!("Grok OAuth response is missing {field}"))
        })
}

fn verification_base_uri(value: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(value.trim()).ok()?;
    url.set_query(None);
    Some(url.to_string())
}

fn timestamp_rfc3339(timestamp: i64) -> Result<String, GrokOAuthError> {
    OffsetDateTime::from_unix_timestamp(timestamp)
        .map_err(|_| {
            GrokOAuthError::InvalidResponse("Grok OAuth timestamp is out of range".to_owned())
        })?
        .format(&Rfc3339)
        .map_err(|_| {
            GrokOAuthError::InvalidResponse("Grok OAuth timestamp is out of range".to_owned())
        })
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct DiscoveryResponse {
    issuer: String,
    device_authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    verification_uri_complete: Option<String>,
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    interval: i64,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    error: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: Option<String>,
    #[serde(default)]
    expires_in: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserResponse {
    user_id: String,
}

#[derive(Debug, Error)]
pub enum GrokOAuthError {
    #[error("{0}")]
    Request(&'static str),
    #[error("{0}")]
    InvalidResponse(String),
    #[error("Grok device authorization expired")]
    Expired,
    #[error("Grok device authorization was denied")]
    Denied,
    #[error("Grok device token request failed with an unsupported OAuth error")]
    OAuth,
    #[error("{0}")]
    Credential(#[source] GrokAuthError),
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Router,
        routing::{get, post},
    };
    use secrecy::ExposeSecret;

    use super::*;

    #[tokio::test]
    async fn completes_device_flow_after_pending_and_slow_down() {
        let token_attempt = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind OAuth endpoint");
        let address = listener.local_addr().expect("OAuth endpoint address");
        let base_url = format!("http://{address}");
        let discovery_body = serde_json::json!({
            "issuer": OIDC_ISSUER,
            "device_authorization_endpoint": format!("{base_url}/device"),
            "token_endpoint": format!("{base_url}/token")
        })
        .to_string();
        let app = Router::new()
            .route(
                "/discovery",
                get(move || {
                    let body = discovery_body.clone();
                    async move { body }
                }),
            )
            .route(
                "/device",
                post(|| async {
                    r#"{"device_code":"device-1","user_code":"CODE-1","verification_uri":"https://accounts.x.ai/device","verification_uri_complete":"https://accounts.x.ai/device?user_code=CODE-1","expires_in":600,"interval":0}"#
                }),
            )
            .route(
                "/token",
                post({
                    let token_attempt = token_attempt.clone();
                    move || {
                        let attempt = token_attempt.fetch_add(1, Ordering::SeqCst);
                        async move {
                            match attempt {
                                0 => r#"{"error":"authorization_pending"}"#,
                                1 => r#"{"error":"slow_down"}"#,
                                _ => r#"{"access_token":"access-secret","refresh_token":"refresh-secret","id_token":"id-secret","token_type":"Bearer","expires_in":3600}"#,
                            }
                        }
                    }
                }),
            )
            .route(
                "/user",
                get(|| async { axum::Json(serde_json::json!({"userId": "oauth-user"})) }),
            );
        let server = tokio::spawn(axum::serve(listener, app).into_future());

        let started = GrokOAuthClient::for_test(format!("{base_url}/discovery"), base_url.clone())
            .start()
            .await
            .expect("start OAuth");
        assert_eq!(started.challenge.user_code, "CODE-1");

        let credential = started.pending.complete().await.expect("complete OAuth");
        server.abort();
        let document: serde_json::Value = serde_json::from_str(
            credential
                .to_json()
                .expect("credential JSON")
                .expose_secret(),
        )
        .expect("credential document");
        assert_eq!(document["access_token"], "access-secret");
        assert_eq!(document["refresh_token"], "refresh-secret");
        assert_eq!(document["upstream_user_id"], "oauth-user");
        assert_eq!(document["oidc_issuer"], OIDC_ISSUER);
        assert_eq!(document["oidc_client_id"], OIDC_CLIENT_ID);
        assert_eq!(token_attempt.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn maps_terminal_device_authorization_errors() {
        let expired = terminal_oauth_error("expired_token").await;
        assert!(matches!(expired, GrokOAuthError::Expired));

        let denied = terminal_oauth_error("access_denied").await;
        assert!(matches!(denied, GrokOAuthError::Denied));
    }

    async fn terminal_oauth_error(error_code: &'static str) -> GrokOAuthError {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind OAuth endpoint");
        let address = listener.local_addr().expect("OAuth endpoint address");
        let base_url = format!("http://{address}");
        let discovery_body = serde_json::json!({
            "issuer": OIDC_ISSUER,
            "device_authorization_endpoint": format!("{base_url}/device"),
            "token_endpoint": format!("{base_url}/token")
        })
        .to_string();
        let token_body = serde_json::json!({"error": error_code}).to_string();
        let app = Router::new()
            .route(
                "/discovery",
                get(move || {
                    let body = discovery_body.clone();
                    async move { body }
                }),
            )
            .route(
                "/device",
                post(|| async {
                    r#"{"device_code":"device-1","user_code":"CODE-1","verification_uri":"https://accounts.x.ai/device","expires_in":600,"interval":1}"#
                }),
            )
            .route(
                "/token",
                post(move || {
                    let body = token_body.clone();
                    async move { body }
                }),
            );
        let server = tokio::spawn(axum::serve(listener, app).into_future());
        let started = GrokOAuthClient::for_test(format!("{base_url}/discovery"), base_url)
            .start()
            .await
            .expect("start OAuth");

        let error = started
            .pending
            .complete()
            .await
            .expect_err("terminal OAuth error should fail");
        server.abort();
        error
    }
}

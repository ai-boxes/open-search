use std::fmt;

use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{
    contract::{OIDC_CLIENT_ID, OIDC_ISSUER},
    refresh::RefreshedGrokTokens,
};

#[derive(Clone)]
pub struct GrokCredentials {
    document: Map<String, Value>,
    access_token: SecretString,
    refresh_token: Option<SecretString>,
    upstream_user_id: Option<String>,
    oidc_issuer: Option<String>,
    oidc_client_id: Option<String>,
}

impl GrokCredentials {
    pub fn from_json(credential_json: &SecretString) -> Result<Self, GrokAuthError> {
        let document: Value = serde_json::from_str(credential_json.expose_secret())?;
        let document = document
            .as_object()
            .cloned()
            .ok_or(GrokAuthError::NotObject)?;
        Self::from_document(document)
    }

    pub(crate) fn refresh_token(&self) -> Option<&SecretString> {
        self.refresh_token.as_ref()
    }

    pub(crate) const fn access_token(&self) -> &SecretString {
        &self.access_token
    }

    pub(crate) fn upstream_user_id(&self) -> Option<&str> {
        self.upstream_user_id.as_deref()
    }

    pub(crate) fn oidc_issuer(&self) -> Option<&str> {
        self.oidc_issuer.as_deref()
    }

    pub(crate) fn oidc_client_id(&self) -> Option<&str> {
        self.oidc_client_id.as_deref()
    }

    pub fn has_supported_refresh_provenance(&self) -> bool {
        self.refresh_token.is_some()
            && self
                .oidc_issuer()
                .is_some_and(|issuer| issuer.trim_end_matches('/') == OIDC_ISSUER)
            && self.oidc_client_id() == Some(OIDC_CLIENT_ID)
    }

    pub fn expires_at(&self) -> Result<Option<i64>, GrokAuthError> {
        timestamp_field(&self.document, "expired")
    }

    pub fn last_refreshed_at(&self) -> Result<Option<i64>, GrokAuthError> {
        timestamp_field(&self.document, "last_refresh")
    }

    pub(crate) fn refreshed(
        &self,
        tokens: &RefreshedGrokTokens,
        refreshed_at: i64,
    ) -> Result<Self, GrokAuthError> {
        let expires_at = refreshed_at
            .checked_add(i64::from(tokens.expires_in))
            .ok_or(GrokAuthError::TimestampOutOfRange)?;
        let mut document = self.document.clone();

        document.insert("type".to_owned(), Value::String("xai".to_owned()));
        document.insert("auth_kind".to_owned(), Value::String("oauth".to_owned()));
        document.insert(
            "access_token".to_owned(),
            Value::String(tokens.access_token.expose_secret().to_owned()),
        );
        if let Some(refresh_token) = tokens.refresh_token.as_ref() {
            document.insert(
                "refresh_token".to_owned(),
                Value::String(refresh_token.expose_secret().to_owned()),
            );
        }
        if let Some(id_token) = tokens.id_token.as_ref() {
            document.insert(
                "id_token".to_owned(),
                Value::String(id_token.expose_secret().to_owned()),
            );
        }
        if let Some(token_type) = tokens.token_type.as_ref() {
            document.insert("token_type".to_owned(), Value::String(token_type.clone()));
        }
        document.insert("expires_in".to_owned(), Value::from(tokens.expires_in));
        document.insert(
            "token_endpoint".to_owned(),
            Value::String(tokens.token_endpoint.clone()),
        );
        document.insert(
            "expired".to_owned(),
            Value::String(timestamp_rfc3339(expires_at)?),
        );
        document.insert(
            "last_refresh".to_owned(),
            Value::String(timestamp_rfc3339(refreshed_at)?),
        );
        document.insert("disabled".to_owned(), Value::Bool(false));

        Self::from_document(document)
    }

    pub(crate) fn to_json(&self) -> Result<SecretString, GrokAuthError> {
        serde_json::to_string(&Value::Object(self.document.clone()))
            .map(SecretString::from)
            .map_err(GrokAuthError::Json)
    }

    fn from_document(document: Map<String, Value>) -> Result<Self, GrokAuthError> {
        if !string_field(&document, "type")
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("xai"))
        {
            return Err(GrokAuthError::InvalidProviderType);
        }
        if !string_field(&document, "auth_kind")
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("oauth"))
        {
            return Err(GrokAuthError::InvalidAuthKind);
        }
        if document
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or_default()
        {
            return Err(GrokAuthError::Disabled);
        }

        let access_token =
            required_secret(&document, "access_token").ok_or(GrokAuthError::MissingAccessToken)?;
        let refresh_token = optional_secret(&document, "refresh_token");
        let upstream_user_id = normalized_string_field(&document, "upstream_user_id");
        let oidc_issuer = normalized_string_field(&document, "oidc_issuer");
        let oidc_client_id = normalized_string_field(&document, "oidc_client_id");

        Ok(Self {
            document,
            access_token,
            refresh_token,
            upstream_user_id,
            oidc_issuer,
            oidc_client_id,
        })
    }
}

impl fmt::Debug for GrokCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GrokCredentials")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("oidc_issuer", &self.oidc_issuer)
            .field("oidc_client_id", &self.oidc_client_id)
            .field("upstream_user_id", &self.upstream_user_id)
            .finish_non_exhaustive()
    }
}

fn string_field<'a>(document: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    document.get(field).and_then(Value::as_str)
}

fn normalized_string_field(document: &Map<String, Value>, field: &str) -> Option<String> {
    string_field(document, field)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn required_secret(document: &Map<String, Value>, field: &str) -> Option<SecretString> {
    optional_secret(document, field).filter(|value| !value.expose_secret().trim().is_empty())
}

fn optional_secret(document: &Map<String, Value>, field: &str) -> Option<SecretString> {
    string_field(document, field)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| SecretString::from(value.to_owned()))
}

fn timestamp_rfc3339(timestamp: i64) -> Result<String, GrokAuthError> {
    OffsetDateTime::from_unix_timestamp(timestamp)
        .map_err(|_| GrokAuthError::TimestampOutOfRange)?
        .format(&Rfc3339)
        .map_err(|_| GrokAuthError::TimestampOutOfRange)
}

fn timestamp_field(
    document: &Map<String, Value>,
    field: &str,
) -> Result<Option<i64>, GrokAuthError> {
    let Some(value) = string_field(document, field)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    OffsetDateTime::parse(value, &Rfc3339)
        .map(|timestamp| Some(timestamp.unix_timestamp()))
        .map_err(|_| GrokAuthError::InvalidTimestamp(field.to_owned()))
}

#[derive(Debug, Error)]
pub enum GrokAuthError {
    #[error("failed to parse Grok auth JSON")]
    Json(#[source] serde_json::Error),
    #[error("Grok auth JSON must be an object")]
    NotObject,
    #[error("Grok auth JSON must have type xai")]
    InvalidProviderType,
    #[error("Grok auth JSON must have auth_kind oauth")]
    InvalidAuthKind,
    #[error("Grok credential is disabled")]
    Disabled,
    #[error("Grok auth JSON is missing access_token")]
    MissingAccessToken,
    #[error("Grok credential timestamp is out of range")]
    TimestampOutOfRange,
    #[error("Grok auth JSON has invalid {0} timestamp")]
    InvalidTimestamp(String),
}

impl From<serde_json::Error> for GrokAuthError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_auth_without_exposing_token_in_debug() {
        let credentials = GrokCredentials::from_json(&SecretString::from(
            r#"{"type":"xai","auth_kind":"oauth","access_token":"secret-token","disabled":false}"#,
        ))
        .expect("valid credentials");
        let debug = format!("{credentials:?}");

        assert!(!debug.contains("secret-token"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn parse_error_does_not_echo_secret_value() {
        let error = GrokCredentials::from_json(&SecretString::from(
            r#"{"type":"xai","auth_kind":"oauth","access_token":"do-not-log""#,
        ))
        .expect_err("invalid JSON");

        assert!(!error.to_string().contains("do-not-log"));
    }

    #[test]
    fn refresh_keeps_unrotated_refresh_token() {
        let credentials = GrokCredentials::from_json(&SecretString::from(format!(
            r#"{{"type":"xai","auth_kind":"oauth","access_token":"old-access","refresh_token":"old-refresh","oidc_issuer":"{OIDC_ISSUER}","oidc_client_id":"{OIDC_CLIENT_ID}"}}"#
        )))
        .expect("credentials");
        let refreshed = credentials
            .refreshed(
                &RefreshedGrokTokens {
                    access_token: SecretString::from("new-access"),
                    refresh_token: None,
                    id_token: None,
                    token_type: Some("Bearer".to_owned()),
                    expires_in: 3600,
                    token_endpoint: "https://auth.x.ai/token".to_owned(),
                },
                1_700_000_000,
            )
            .expect("refreshed credential");
        let document: Value = serde_json::from_str(
            refreshed
                .to_json()
                .expect("credential JSON")
                .expose_secret(),
        )
        .expect("credential document");

        assert_eq!(document["access_token"], "new-access");
        assert_eq!(document["refresh_token"], "old-refresh");
        assert_eq!(document["oidc_issuer"], OIDC_ISSUER);
        assert_eq!(document["oidc_client_id"], OIDC_CLIENT_ID);
    }
}

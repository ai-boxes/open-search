use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::{Date, Month};

pub const MAX_WEB_SEARCH_DOMAINS: usize = 5;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchCapability {
    X,
    Web,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SearchRequest {
    X(XSearchRequest),
    Web(WebSearchRequest),
}

impl SearchRequest {
    pub fn validate(self) -> Result<ValidatedSearchRequest, SearchError> {
        match self {
            Self::X(request) => request.validate().map(ValidatedSearchRequest::X),
            Self::Web(request) => request.validate().map(ValidatedSearchRequest::Web),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct XSearchRequest {
    pub query: String,
    #[serde(default)]
    pub from_date: Option<String>,
    #[serde(default)]
    pub to_date: Option<String>,
}

impl XSearchRequest {
    fn validate(self) -> Result<ValidatedXSearchRequest, SearchError> {
        let from_date = validate_date_bound("from_date", self.from_date)?;
        let to_date = validate_date_bound("to_date", self.to_date)?;
        if let (Some((from_value, from)), Some((to_value, to))) = (&from_date, &to_date)
            && from > to
        {
            return Err(SearchError::InvalidRequest(format!(
                "from_date must not be after to_date: {from_value} > {to_value}"
            )));
        }
        Ok(ValidatedXSearchRequest {
            query: validate_query(self.query)?,
            from_date: from_date.map(|(value, _)| value),
            to_date: to_date.map(|(value, _)| value),
        })
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WebSearchRequest {
    pub query: String,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default)]
    pub excluded_domains: Vec<String>,
}

impl WebSearchRequest {
    fn validate(self) -> Result<ValidatedWebSearchRequest, SearchError> {
        let allowed_domains = normalize_list(self.allowed_domains);
        let excluded_domains = normalize_list(self.excluded_domains);
        if !allowed_domains.is_empty() && !excluded_domains.is_empty() {
            return Err(SearchError::InvalidRequest(
                "allowed_domains and excluded_domains cannot both be set".to_owned(),
            ));
        }
        for (field, values) in [
            ("allowed_domains", &allowed_domains),
            ("excluded_domains", &excluded_domains),
        ] {
            if values.len() > MAX_WEB_SEARCH_DOMAINS {
                return Err(SearchError::InvalidRequest(format!(
                    "{field} must contain at most {MAX_WEB_SEARCH_DOMAINS} entries"
                )));
            }
        }
        Ok(ValidatedWebSearchRequest {
            query: validate_query(self.query)?,
            allowed_domains,
            excluded_domains,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedSearchRequest {
    X(ValidatedXSearchRequest),
    Web(ValidatedWebSearchRequest),
}

impl ValidatedSearchRequest {
    pub fn capability(&self) -> SearchCapability {
        match self {
            Self::X(_) => SearchCapability::X,
            Self::Web(_) => SearchCapability::Web,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedXSearchRequest {
    pub query: String,
    pub from_date: Option<String>,
    pub to_date: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedWebSearchRequest {
    pub query: String,
    pub allowed_domains: Vec<String>,
    pub excluded_domains: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SearchResponse {
    pub results: Vec<serde_json::Value>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SearchError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("search provider is unavailable: {0}")]
    ProviderUnavailable(String),
    #[error("search capability is unavailable: {0:?}")]
    CapabilityUnavailable(SearchCapability),
    #[error("provider authorization has not been completed")]
    NotAuthorized,
    #[error("provider authorization must be completed again")]
    ReauthorizationRequired,
    #[error("upstream provider rate limit exceeded")]
    RateLimited,
    #[error("search request timed out")]
    Timeout,
    #[error("upstream provider rejected the request: {0}")]
    UpstreamRejected(String),
    #[error("upstream provider is temporarily unavailable: {0}")]
    UpstreamUnavailable(String),
}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn supports(&self, capability: SearchCapability) -> bool;

    async fn search(&self, request: ValidatedSearchRequest) -> Result<SearchResponse, SearchError>;
}

fn validate_query(query: String) -> Result<String, SearchError> {
    if query.trim().is_empty() {
        return Err(SearchError::InvalidRequest(
            "query must not be empty".to_owned(),
        ));
    }

    Ok(query)
}

fn validate_date_bound(
    field: &str,
    value: Option<String>,
) -> Result<Option<(String, Date)>, SearchError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| !matches!(index, 4 | 7) && !byte.is_ascii_digit())
    {
        return Err(SearchError::InvalidRequest(format!(
            "{field} must use YYYY-MM-DD"
        )));
    }
    let year = value[0..4]
        .parse::<i32>()
        .map_err(|_| SearchError::InvalidRequest(format!("{field} must use YYYY-MM-DD")))?;
    let month = value[5..7]
        .parse::<u8>()
        .ok()
        .and_then(|month| Month::try_from(month).ok())
        .ok_or_else(|| SearchError::InvalidRequest(format!("{field} contains an invalid date")))?;
    let day = value[8..10]
        .parse::<u8>()
        .map_err(|_| SearchError::InvalidRequest(format!("{field} contains an invalid date")))?;
    let date = Date::from_calendar_date(year, month, day)
        .map_err(|_| SearchError::InvalidRequest(format!("{field} contains an invalid date")))?;
    Ok(Some((value, date)))
}

fn normalize_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .fold(Vec::new(), |mut normalized, value| {
            let value = value.trim().to_owned();
            if !value.is_empty() && !normalized.contains(&value) {
                normalized.push(value);
            }
            normalized
        })
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_WEB_SEARCH_DOMAINS, SearchCapability, SearchError, SearchRequest,
        ValidatedSearchRequest, WebSearchRequest, XSearchRequest,
    };

    #[test]
    fn validates_and_normalizes_x_request() {
        let request = SearchRequest::X(XSearchRequest {
            query: "  rust async  ".to_owned(),
            from_date: Some("2026-01-01".to_owned()),
            to_date: None,
        })
        .validate()
        .expect("request should be valid");

        let ValidatedSearchRequest::X(request) = request else {
            panic!("request should remain an X search");
        };
        assert_eq!(request.query, "  rust async  ");
        assert_eq!(request.from_date.as_deref(), Some("2026-01-01"));
        assert_eq!(request.to_date, None);
    }

    #[test]
    fn validates_and_normalizes_web_request() {
        let request = SearchRequest::Web(WebSearchRequest {
            query: "  Rust releases ".to_owned(),
            allowed_domains: vec![" rust-lang.org ".to_owned()],
            excluded_domains: Vec::new(),
        })
        .validate()
        .expect("request should be valid");

        assert_eq!(request.capability(), SearchCapability::Web);
        let ValidatedSearchRequest::Web(request) = request else {
            panic!("request should remain a Web search");
        };
        assert_eq!(request.query, "  Rust releases ");
        assert_eq!(request.allowed_domains, ["rust-lang.org"]);
        assert!(request.excluded_domains.is_empty());
    }

    #[test]
    fn rejects_empty_query() {
        let error = SearchRequest::Web(WebSearchRequest {
            query: " ".to_owned(),
            allowed_domains: Vec::new(),
            excluded_domains: Vec::new(),
        })
        .validate()
        .expect_err("query should be rejected");

        assert!(matches!(error, SearchError::InvalidRequest(_)));
    }

    #[test]
    fn rejects_invalid_web_domain_filters() {
        let both = SearchRequest::Web(WebSearchRequest {
            query: "rust".to_owned(),
            allowed_domains: vec!["rust-lang.org".to_owned()],
            excluded_domains: vec!["example.com".to_owned()],
        })
        .validate()
        .expect_err("allow and block lists should be mutually exclusive");
        assert!(matches!(both, SearchError::InvalidRequest(_)));

        let too_many = SearchRequest::Web(WebSearchRequest {
            query: "rust".to_owned(),
            allowed_domains: (0..=MAX_WEB_SEARCH_DOMAINS)
                .map(|index| format!("{index}.example.com"))
                .collect(),
            excluded_domains: Vec::new(),
        })
        .validate()
        .expect_err("domain list should be capped");
        assert!(matches!(too_many, SearchError::InvalidRequest(_)));
    }

    #[test]
    fn validates_x_date_bounds() {
        let invalid = SearchRequest::X(XSearchRequest {
            query: "rust".to_owned(),
            from_date: Some("2026-02-30".to_owned()),
            to_date: None,
        })
        .validate()
        .expect_err("invalid date should be rejected");
        assert!(matches!(invalid, SearchError::InvalidRequest(_)));

        let inverted = SearchRequest::X(XSearchRequest {
            query: "rust".to_owned(),
            from_date: Some("2026-08-29".to_owned()),
            to_date: Some("2026-01-01".to_owned()),
        })
        .validate()
        .expect_err("inverted range should be rejected");
        assert!(matches!(inverted, SearchError::InvalidRequest(_)));
    }
}

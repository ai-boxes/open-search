use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_MAX_RESULTS: usize = 8;
pub const MAX_RESULTS_LIMIT: usize = 20;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<usize>,
}

impl SearchRequest {
    pub fn validate(self) -> Result<ValidatedSearchRequest, SearchError> {
        let query = self.query.trim().to_owned();
        if query.is_empty() {
            return Err(SearchError::InvalidRequest(
                "query must not be empty".to_owned(),
            ));
        }

        let max_results = self.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
        if !(1..=MAX_RESULTS_LIMIT).contains(&max_results) {
            return Err(SearchError::InvalidRequest(format!(
                "max_results must be between 1 and {MAX_RESULTS_LIMIT}"
            )));
        }

        Ok(ValidatedSearchRequest { query, max_results })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSearchRequest {
    pub query: String,
    pub max_results: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnswerRequest {
    pub query: String,
    pub sources: Vec<SearchHit>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GeneratedAnswer {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchResponse {
    pub answer: String,
    pub sources: Vec<SearchHit>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SearchError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("search provider is unavailable: {0}")]
    SearchProviderUnavailable(String),
    #[error("answer provider is unavailable: {0}")]
    AnswerProviderUnavailable(String),
    #[error("upstream provider rejected the request: {0}")]
    Upstream(String),
}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    async fn search(&self, request: &ValidatedSearchRequest)
    -> Result<Vec<SearchHit>, SearchError>;
}

#[async_trait]
pub trait AnswerProvider: Send + Sync {
    async fn answer(&self, request: AnswerRequest) -> Result<GeneratedAnswer, SearchError>;
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_MAX_RESULTS, MAX_RESULTS_LIMIT, SearchError, SearchRequest};

    #[test]
    fn validates_and_normalizes_request() {
        let request = SearchRequest {
            query: "  rust async  ".to_owned(),
            max_results: None,
        }
        .validate()
        .expect("request should be valid");

        assert_eq!(request.query, "rust async");
        assert_eq!(request.max_results, DEFAULT_MAX_RESULTS);
    }

    #[test]
    fn rejects_out_of_range_result_limit() {
        let error = SearchRequest {
            query: "rust".to_owned(),
            max_results: Some(MAX_RESULTS_LIMIT + 1),
        }
        .validate()
        .expect_err("limit should be rejected");

        assert!(matches!(error, SearchError::InvalidRequest(_)));
    }
}

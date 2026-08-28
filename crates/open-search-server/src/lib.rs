use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use open_search_core::{SearchError, SearchRequest};
use open_search_runtime::SearchEngine;
use serde::Serialize;

pub fn app(engine: SearchEngine) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/search", post(search))
        .with_state(engine)
}

async fn healthz() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn search(
    State(engine): State<SearchEngine>,
    Json(request): Json<SearchRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let response = engine.search(request).await?;
    Ok((StatusCode::OK, Json(response)))
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}

struct ApiError(SearchError);

impl From<SearchError> for ApiError {
    fn from(error: SearchError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match self.0 {
            SearchError::InvalidRequest(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
            SearchError::SearchProviderUnavailable(_)
            | SearchError::AnswerProviderUnavailable(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, "provider_unavailable")
            }
            SearchError::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream_error"),
        };
        let body = ErrorBody {
            error: ErrorDetail {
                code,
                message: self.0.to_string(),
            },
        };

        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use open_search_core::{
        AnswerProvider, AnswerRequest, GeneratedAnswer, SearchError, SearchHit, SearchProvider,
        ValidatedSearchRequest,
    };
    use open_search_runtime::SearchEngine;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::app;

    struct FakeSearchProvider;

    #[async_trait]
    impl SearchProvider for FakeSearchProvider {
        async fn search(
            &self,
            _request: &ValidatedSearchRequest,
        ) -> Result<Vec<SearchHit>, SearchError> {
            Ok(vec![SearchHit {
                title: "Example".to_owned(),
                url: "https://example.com".to_owned(),
                snippet: "Example evidence".to_owned(),
                score: None,
            }])
        }
    }

    struct FakeAnswerProvider;

    #[async_trait]
    impl AnswerProvider for FakeAnswerProvider {
        async fn answer(&self, _request: AnswerRequest) -> Result<GeneratedAnswer, SearchError> {
            Ok(GeneratedAnswer {
                text: "Example answer".to_owned(),
            })
        }
    }

    #[tokio::test]
    async fn serves_search_response() {
        let engine = SearchEngine::new(Arc::new(FakeSearchProvider), Arc::new(FakeAnswerProvider));
        let response = app(engine)
            .oneshot(
                Request::post("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":"example"}"#))
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        let body: Value = serde_json::from_slice(&body).expect("body should be JSON");
        assert_eq!(body["answer"], "Example answer");
        assert_eq!(body["sources"].as_array().map(Vec::len), Some(1));
    }
}

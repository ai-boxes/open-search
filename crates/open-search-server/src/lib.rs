mod mcp;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use open_search_core::{SearchError, SearchRequest};
use open_search_runtime::SearchEngine;
use serde::Serialize;

use self::mcp::mcp_service;

pub fn app(engine: SearchEngine, bearer_token: Option<String>) -> Router {
    let protected_routes = Router::new()
        .route("/v1/search", post(search))
        .nest_service("/mcp", mcp_service(engine.clone()))
        .with_state(engine)
        .layer(middleware::from_fn_with_state(
            bearer_token,
            require_bearer_token,
        ));

    Router::new()
        .route("/healthz", get(healthz))
        .merge(protected_routes)
}

async fn healthz() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn require_bearer_token(
    State(expected_token): State<Option<String>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected_token) = expected_token else {
        return next.run(request).await;
    };
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .is_some_and(|(scheme, token)| {
            scheme.eq_ignore_ascii_case("Bearer") && token == expected_token
        });
    if authorized {
        return next.run(request).await;
    }

    let mut response = StatusCode::UNAUTHORIZED.into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        header::HeaderValue::from_static("Bearer"),
    );
    response
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
            SearchError::NotAuthorized => (StatusCode::UNAUTHORIZED, "not_authorized"),
            SearchError::ReauthorizationRequired => {
                (StatusCode::UNAUTHORIZED, "reauthorization_required")
            }
            SearchError::CapabilityUnavailable(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, "capability_unavailable")
            }
            SearchError::ProviderUnavailable(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, "provider_unavailable")
            }
            SearchError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            SearchError::Timeout => (StatusCode::GATEWAY_TIMEOUT, "timeout"),
            SearchError::UpstreamRejected(_) => (StatusCode::BAD_GATEWAY, "upstream_rejected"),
            SearchError::UpstreamUnavailable(_) => {
                (StatusCode::BAD_GATEWAY, "upstream_unavailable")
            }
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
    use std::{collections::HashMap, sync::Arc};

    use async_trait::async_trait;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use open_search_core::{
        SearchCapability, SearchError, SearchProvider, SearchResponse, ValidatedSearchRequest,
    };
    use open_search_runtime::SearchEngine;
    use rmcp::{
        ServiceExt as _,
        model::{CallToolRequestParams, ClientInfo},
        transport::{
            StreamableHttpClientTransport,
            streamable_http_client::StreamableHttpClientTransportConfig,
        },
    };
    use serde_json::Value;
    use tower::ServiceExt;

    use super::app;

    struct FakeSearchProvider;

    #[async_trait]
    impl SearchProvider for FakeSearchProvider {
        fn supports(&self, capability: SearchCapability) -> bool {
            matches!(capability, SearchCapability::X | SearchCapability::Web)
        }

        async fn search(
            &self,
            request: ValidatedSearchRequest,
        ) -> Result<SearchResponse, SearchError> {
            let (kind, query) = match request {
                ValidatedSearchRequest::X(request) => ("x", request.query),
                ValidatedSearchRequest::Web(request) => ("web", request.query),
            };
            if query == "unauthorized" {
                return Err(SearchError::NotAuthorized);
            }
            Ok(SearchResponse {
                results: vec![serde_json::json!({
                    "type": "hosted_tool_call",
                    "kind": kind,
                    "query": query
                })],
            })
        }
    }

    #[tokio::test]
    async fn serves_web_search_response() {
        let engine = SearchEngine::new(Arc::new(FakeSearchProvider));
        let response = app(engine, None)
            .oneshot(
                Request::post("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"type":"web","query":"example"}"#))
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        let body: Value = serde_json::from_slice(&body).expect("body should be JSON");
        assert_eq!(body["results"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["results"][0]["kind"], "web");
    }

    #[tokio::test]
    async fn serves_x_search_response() {
        let engine = SearchEngine::new(Arc::new(FakeSearchProvider));
        let response = app(engine, None)
            .oneshot(
                Request::post("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"type":"x","query":"example"}"#))
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        let body: Value = serde_json::from_slice(&body).expect("body should be JSON");
        assert_eq!(body["results"][0]["kind"], "x");
    }

    #[tokio::test]
    async fn rejects_invalid_domain_filters() {
        let engine = SearchEngine::new(Arc::new(FakeSearchProvider));
        let response = app(engine, None)
            .oneshot(
                Request::post("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"type":"web","query":"example","allowed_domains":["example.com"],"excluded_domains":["blocked.example"]}"#))
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        let body: Value = serde_json::from_slice(&body).expect("body should be JSON");
        assert_eq!(body["error"]["code"], "invalid_request");
    }

    #[tokio::test]
    async fn bearer_token_protects_search_endpoints_but_not_health() {
        let engine = SearchEngine::new(Arc::new(FakeSearchProvider));
        let app = app(engine, Some("downstream-secret".to_owned()));

        let health = app
            .clone()
            .oneshot(
                Request::get("/healthz")
                    .body(Body::empty())
                    .expect("health request should build"),
            )
            .await
            .expect("health request should complete");
        assert_eq!(health.status(), StatusCode::NO_CONTENT);

        let unauthorized_mcp = app
            .clone()
            .oneshot(
                Request::post("/mcp")
                    .body(Body::empty())
                    .expect("MCP request should build"),
            )
            .await
            .expect("MCP request should complete");
        assert_eq!(unauthorized_mcp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthorized_mcp
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer")
        );

        let unauthorized_search = app
            .clone()
            .oneshot(
                Request::post("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"type":"web","query":"example"}"#))
                    .expect("search request should build"),
            )
            .await
            .expect("search request should complete");
        assert_eq!(unauthorized_search.status(), StatusCode::UNAUTHORIZED);

        let authorized_search = app
            .oneshot(
                Request::post("/v1/search")
                    .header("authorization", "Bearer downstream-secret")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"type":"web","query":"example"}"#))
                    .expect("authorized search request should build"),
            )
            .await
            .expect("authorized search request should complete");
        assert_eq!(authorized_search.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn authenticated_mcp_client_discovers_and_calls_only_grok_search_tools() {
        let engine = SearchEngine::new(Arc::new(FakeSearchProvider));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener");
        let address = listener.local_addr().expect("test address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app(engine, Some("downstream-secret".to_owned()))).await
        });
        let mut headers = HashMap::new();
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_static("Bearer downstream-secret"),
        );
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp"))
                .custom_headers(headers),
        );
        let client = ClientInfo::default()
            .serve(transport)
            .await
            .expect("MCP initialize");

        let tools = client.list_tools(None).await.expect("list MCP tools");
        let mut names = tools
            .tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(names, ["grok_web_search", "grok_x_search"]);
        for tool in &tools.tools {
            let schema = serde_json::to_value(&tool.input_schema).expect("tool input schema");
            let properties = &schema["properties"];
            for unsupported in ["model", "handles", "recency", "max_results", "domains"] {
                assert!(properties.get(unsupported).is_none());
            }
            assert!(
                schema["required"]
                    .as_array()
                    .is_some_and(|required| required.contains(&serde_json::json!("query")))
            );
        }

        let arguments = serde_json::from_value(serde_json::json!({
            "query": "example",
            "allowed_domains": ["example.com"]
        }))
        .expect("tool arguments");
        let result = client
            .call_tool(CallToolRequestParams::new("grok_web_search").with_arguments(arguments))
            .await
            .expect("call MCP tool");

        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            result
                .structured_content
                .as_ref()
                .map(|value| &value["results"][0]["query"]),
            Some(&serde_json::json!("example"))
        );
        assert_eq!(
            result
                .structured_content
                .as_ref()
                .map(|value| &value["results"][0]["kind"]),
            Some(&serde_json::json!("web"))
        );

        let invalid_arguments = serde_json::from_value(serde_json::json!({
            "query": "example",
            "allowed_domains": ["example.com"],
            "excluded_domains": ["blocked.example"]
        }))
        .expect("invalid tool arguments");
        let invalid = client
            .call_tool(
                CallToolRequestParams::new("grok_web_search").with_arguments(invalid_arguments),
            )
            .await
            .expect("invalid tool call result");
        assert_eq!(invalid.is_error, Some(true));
        assert_eq!(
            invalid
                .structured_content
                .as_ref()
                .map(|value| &value["error"]["code"]),
            Some(&serde_json::json!("invalid_request"))
        );

        let unauthorized_arguments = serde_json::from_value(serde_json::json!({
            "query": "unauthorized"
        }))
        .expect("unauthorized tool arguments");
        let unauthorized = client
            .call_tool(
                CallToolRequestParams::new("grok_x_search").with_arguments(unauthorized_arguments),
            )
            .await
            .expect("unauthorized tool call result");
        assert_eq!(unauthorized.is_error, Some(true));
        assert_eq!(
            unauthorized
                .structured_content
                .as_ref()
                .map(|value| &value["error"]["code"]),
            Some(&serde_json::json!("not_authorized"))
        );

        let _ = client.cancel().await;
        server.abort();
    }
}

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::StreamExt;
use open_search_core::{
    SearchCapability, SearchError, SearchProvider, SearchResponse, ValidatedSearchRequest,
    ValidatedWebSearchRequest, ValidatedXSearchRequest,
};
use reqwest::{Response, StatusCode};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tracing::{info, warn};
use uuid::Uuid;

use super::{
    contract::PROXY_BASE_URL,
    credentials::GrokCredentials,
    identity::inference_headers,
    session::GrokCredentialSession,
    sse::{GrokSseCollector, GrokSseError, GrokSseOutput},
};

const MODEL: &str = "grok-4.6";
// Search-only requests favor time-to-result over the catalog's general-purpose high default.
const REASONING_EFFORT: &str = "low";
const UPSTREAM_SESSION_TTL: Duration = Duration::from_secs(4 * 60 * 60);
const RESPONSE_HEADERS_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_ERROR_BODY_SIZE: usize = 64 * 1024;

#[derive(Clone)]
pub struct GrokSearchProvider {
    session: GrokCredentialSession,
    client: GrokResponsesClient,
}

impl GrokSearchProvider {
    pub fn new(session: GrokCredentialSession, agent_id: impl Into<String>) -> Self {
        Self {
            session,
            client: GrokResponsesClient::new(PROXY_BASE_URL, agent_id),
        }
    }

    #[cfg(test)]
    fn for_test(
        session: GrokCredentialSession,
        base_url: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            session,
            client: GrokResponsesClient::new(base_url, agent_id),
        }
    }

    async fn execute(
        &self,
        request: &ValidatedSearchRequest,
    ) -> Result<GrokSseOutput, SearchError> {
        let context = self.client.next_request_context();
        let credentials = self.session.credentials().await?;
        match self.client.execute(&credentials, request, &context).await {
            Err(ExecuteError::Unauthorized) => {
                let credentials = self.session.refresh_after_unauthorized().await?;
                self.client
                    .execute(&credentials, request, &context)
                    .await
                    .map_err(|error| map_execute_error(error, request.capability()))
            }
            result => result.map_err(|error| map_execute_error(error, request.capability())),
        }
    }
}

#[async_trait]
impl SearchProvider for GrokSearchProvider {
    fn supports(&self, capability: SearchCapability) -> bool {
        matches!(capability, SearchCapability::X | SearchCapability::Web)
    }

    async fn search(&self, request: ValidatedSearchRequest) -> Result<SearchResponse, SearchError> {
        let started = Instant::now();
        let capability = request.capability();
        let output = match self.execute(&request).await {
            Ok(output) => output,
            Err(error) => {
                warn!(
                    provider = "grok",
                    capability = capability_name(capability),
                    duration_ms = elapsed_millis(started),
                    error = search_error_code(&error),
                    "search failed"
                );
                return Err(error);
            }
        };
        let response = SearchResponse {
            results: output.results,
        };
        info!(
            provider = "grok",
            capability = capability_name(capability),
            duration_ms = elapsed_millis(started),
            results = response.results.len(),
            "search completed"
        );
        Ok(response)
    }
}

fn capability_name(capability: SearchCapability) -> &'static str {
    match capability {
        SearchCapability::X => "x",
        SearchCapability::Web => "web",
    }
}

fn search_error_code(error: &SearchError) -> &'static str {
    match error {
        SearchError::InvalidRequest(_) => "invalid_request",
        SearchError::ProviderUnavailable(_) => "provider_unavailable",
        SearchError::CapabilityUnavailable(_) => "capability_unavailable",
        SearchError::NotAuthorized => "not_authorized",
        SearchError::ReauthorizationRequired => "reauthorization_required",
        SearchError::RateLimited => "rate_limited",
        SearchError::Timeout => "timeout",
        SearchError::UpstreamRejected(_) => "upstream_rejected",
        SearchError::UpstreamUnavailable(_) => "upstream_unavailable",
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[derive(Clone)]
struct GrokResponsesClient {
    http: reqwest::Client,
    base_url: String,
    agent_id: String,
    sessions: GrokUpstreamSessions,
}

impl GrokResponsesClient {
    fn new(base_url: impl Into<String>, agent_id: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            agent_id: agent_id.into(),
            sessions: GrokUpstreamSessions::new(UPSTREAM_SESSION_TTL),
        }
    }

    fn next_request_context(&self) -> UpstreamRequestContext {
        self.sessions.next_request_context()
    }

    async fn execute(
        &self,
        credentials: &GrokCredentials,
        request: &ValidatedSearchRequest,
        context: &UpstreamRequestContext,
    ) -> Result<GrokSseOutput, ExecuteError> {
        let user_id = credentials
            .upstream_user_id()
            .ok_or(ExecuteError::MissingUserId)?;
        let body = build_request(request, &context.session_id);
        let body = serde_json::to_vec(&body).map_err(|_| ExecuteError::InvalidRequestBody)?;
        let response = inference_headers(
            self.http
                .post(format!("{}/responses", self.base_url))
                .bearer_auth(credentials.access_token().expose_secret())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "text/event-stream"),
        )
        .header("x-grok-user-id", user_id)
        .header("x-grok-conv-id", &context.session_id)
        .header("x-grok-req-id", &context.request_id)
        .header("x-grok-model-override", MODEL)
        .header("x-grok-session-id", &context.session_id)
        .header("x-grok-turn-idx", context.turn_index.to_string())
        .header("x-grok-agent-id", &self.agent_id)
        .body(body);
        let response = tokio::time::timeout(RESPONSE_HEADERS_TIMEOUT, response.send())
            .await
            .map_err(|_| ExecuteError::Timeout)?
            .map_err(|_| ExecuteError::Unavailable)?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(response, status).await);
        }

        collect_stream(response, request.capability()).await
    }
}

fn build_request(request: &ValidatedSearchRequest, prompt_cache_key: &str) -> Value {
    let (query, tool) = match request {
        ValidatedSearchRequest::X(request) => (&request.query, x_search_tool(request)),
        ValidatedSearchRequest::Web(request) => (&request.query, web_search_tool(request)),
    };
    json!({
        "model": MODEL,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content", "no_inline_citations"],
        "reasoning": {"effort": REASONING_EFFORT, "summary": "concise"},
        "prompt_cache_key": prompt_cache_key,
        "input": [{
            "type": "message",
            "role": "user",
            "content": query
        }],
        "tool_choice": "required",
        "tools": [tool]
    })
}

fn x_search_tool(request: &ValidatedXSearchRequest) -> Value {
    let mut tool = serde_json::Map::from_iter([("type".to_owned(), json!("x_search"))]);
    if let Some(from_date) = &request.from_date {
        tool.insert("from_date".to_owned(), json!(from_date));
    }
    if let Some(to_date) = &request.to_date {
        tool.insert("to_date".to_owned(), json!(to_date));
    }
    Value::Object(tool)
}

fn web_search_tool(request: &ValidatedWebSearchRequest) -> Value {
    let mut filters = serde_json::Map::new();
    if !request.allowed_domains.is_empty() {
        filters.insert("allowed_domains".to_owned(), json!(request.allowed_domains));
    }
    if !request.excluded_domains.is_empty() {
        filters.insert(
            "excluded_domains".to_owned(),
            json!(request.excluded_domains),
        );
    }
    if filters.is_empty() {
        json!({"type": "web_search"})
    } else {
        json!({"type": "web_search", "filters": filters})
    }
}

#[derive(Clone)]
struct GrokUpstreamSessions {
    inner: Arc<Mutex<UpstreamSessionState>>,
    ttl: Duration,
}

struct UpstreamSessionState {
    id: String,
    created_at: Instant,
    next_turn_index: u64,
}

struct UpstreamRequestContext {
    session_id: String,
    request_id: String,
    turn_index: u64,
}

impl GrokUpstreamSessions {
    fn new(ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(UpstreamSessionState::new(Instant::now()))),
            ttl,
        }
    }

    fn next_request_context(&self) -> UpstreamRequestContext {
        let now = Instant::now();
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if now.duration_since(state.created_at) >= self.ttl {
            *state = UpstreamSessionState::new(now);
        }
        state.next_turn_index = state.next_turn_index.saturating_add(1);
        UpstreamRequestContext {
            session_id: state.id.clone(),
            request_id: Uuid::new_v4().to_string(),
            turn_index: state.next_turn_index,
        }
    }
}

impl UpstreamSessionState {
    fn new(created_at: Instant) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            created_at,
            next_turn_index: 0,
        }
    }
}

async fn collect_stream(
    response: Response,
    capability: SearchCapability,
) -> Result<GrokSseOutput, ExecuteError> {
    let mut stream = response.bytes_stream();
    let mut collector = GrokSseCollector::new(capability);
    loop {
        let next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_| ExecuteError::Timeout)?;
        match next {
            Some(Ok(chunk)) => {
                collector.push(&chunk).map_err(ExecuteError::Stream)?;
                if let Some(output) = collector.take_output() {
                    return Ok(output);
                }
            }
            Some(Err(_)) => return Err(ExecuteError::Unavailable),
            None => break,
        }
    }
    collector.finish().map_err(ExecuteError::Stream)
}

async fn status_error(response: Response, status: StatusCode) -> ExecuteError {
    if status == StatusCode::UNAUTHORIZED {
        return ExecuteError::Unauthorized;
    }
    let body = collect_error_body(response).await;
    if status == StatusCode::TOO_MANY_REQUESTS {
        return ExecuteError::RateLimited;
    }
    if status.is_server_error() {
        return ExecuteError::Unavailable;
    }
    let body = String::from_utf8_lossy(&body).to_ascii_lowercase();
    if status == StatusCode::FORBIDDEN
        && (body.contains("bad-credentials")
            || body.contains("unauthenticated")
            || body.contains("access token"))
    {
        return ExecuteError::Unauthorized;
    }
    if body.contains("backend search")
        || body.contains("backend_search")
        || (body.contains("model") && body.contains("search"))
    {
        return ExecuteError::SearchUnsupported;
    }
    ExecuteError::Rejected
}

async fn collect_error_body(response: Response) -> Vec<u8> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(Ok(chunk)) = stream.next().await {
        let remaining = MAX_ERROR_BODY_SIZE.saturating_sub(body.len());
        if remaining == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    body
}

fn map_execute_error(error: ExecuteError, capability: SearchCapability) -> SearchError {
    match error {
        ExecuteError::Unauthorized | ExecuteError::MissingUserId => {
            SearchError::ReauthorizationRequired
        }
        ExecuteError::RateLimited => SearchError::RateLimited,
        ExecuteError::Timeout => SearchError::Timeout,
        ExecuteError::SearchUnsupported => SearchError::CapabilityUnavailable(capability),
        ExecuteError::Rejected => {
            SearchError::UpstreamRejected("Grok rejected the search request".to_owned())
        }
        ExecuteError::Unavailable => {
            SearchError::UpstreamUnavailable("Grok is temporarily unavailable".to_owned())
        }
        ExecuteError::InvalidRequestBody => {
            SearchError::ProviderUnavailable("failed to build the Grok request".to_owned())
        }
        ExecuteError::Stream(error) => {
            SearchError::UpstreamUnavailable(format!("invalid Grok response stream: {error}"))
        }
    }
}

#[derive(Debug)]
enum ExecuteError {
    Unauthorized,
    MissingUserId,
    RateLimited,
    Timeout,
    SearchUnsupported,
    Rejected,
    Unavailable,
    InvalidRequestBody,
    Stream(GrokSseError),
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Json, Router,
        body::{Body, to_bytes},
        extract::{Request, State},
        http::{Response as AxumResponse, StatusCode as AxumStatusCode},
        routing::{get, post},
    };
    use secrecy::SecretString;
    use tokio::net::TcpListener;

    use super::*;
    use crate::grok::{
        GrokCredentialStore, GrokRefreshClient,
        contract::{OIDC_CLIENT_ID, OIDC_ISSUER},
    };

    #[derive(Default)]
    struct CapturedRequest {
        authorization: String,
        user_id: String,
        model: String,
        agent_id: String,
        conversation_id: String,
        request_id: String,
        session_id: String,
        turn_index: String,
        body: Value,
    }

    async fn responses_handler(
        State(captured): State<Arc<Mutex<CapturedRequest>>>,
        request: Request,
    ) -> AxumResponse<Body> {
        let headers = request.headers().clone();
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("request body");
        *captured.lock().expect("capture lock") = CapturedRequest {
            authorization: header(&headers, "authorization"),
            user_id: header(&headers, "x-grok-user-id"),
            model: header(&headers, "x-grok-model-override"),
            agent_id: header(&headers, "x-grok-agent-id"),
            conversation_id: header(&headers, "x-grok-conv-id"),
            request_id: header(&headers, "x-grok-req-id"),
            session_id: header(&headers, "x-grok-session-id"),
            turn_index: header(&headers, "x-grok-turn-idx"),
            body: serde_json::from_slice(&body).expect("JSON request"),
        };
        AxumResponse::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"id\":\"web-1\",\"status\":\"completed\",\"action\":{\"type\":\"search\",\"query\":\"Rust\",\"sources\":[{\"url\":\"https://www.rust-lang.org\"}]}}}\n\n",
            ))
            .expect("response")
    }

    fn header(headers: &reqwest::header::HeaderMap, name: &str) -> String {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    #[derive(Clone, Default)]
    struct RetryState {
        base_url: Arc<Mutex<String>>,
        authorizations: Arc<Mutex<Vec<String>>>,
    }

    async fn discovery_handler(State(state): State<RetryState>) -> Json<Value> {
        let base_url = state.base_url.lock().expect("base URL lock").clone();
        Json(json!({
            "issuer": OIDC_ISSUER,
            "token_endpoint": format!("{base_url}/token")
        }))
    }

    async fn token_handler() -> Json<Value> {
        Json(json!({
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "token_type": "Bearer",
            "expires_in": 3600
        }))
    }

    async fn retry_responses_handler(
        State(state): State<RetryState>,
        request: Request,
    ) -> AxumResponse<Body> {
        let authorization = header(request.headers(), "authorization");
        let call_count = {
            let mut authorizations = state.authorizations.lock().expect("authorization lock");
            authorizations.push(authorization);
            authorizations.len()
        };
        if call_count == 1 {
            return AxumResponse::builder()
                .status(AxumStatusCode::UNAUTHORIZED)
                .body(Body::empty())
                .expect("unauthorized response");
        }
        AxumResponse::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from(
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"id\":\"web-retry\",\"status\":\"completed\",\"action\":{\"type\":\"search\",\"query\":\"Rust\",\"sources\":[]}}}\n\n",
            ))
            .expect("successful response")
    }

    #[test]
    fn builds_fixed_model_and_hosted_search_tools() {
        let web = build_request(
            &ValidatedSearchRequest::Web(ValidatedWebSearchRequest {
                query: "  Rust  ".to_owned(),
                allowed_domains: vec!["rust-lang.org".to_owned()],
                excluded_domains: Vec::new(),
            }),
            "session-123",
        );
        let x = build_request(
            &ValidatedSearchRequest::X(ValidatedXSearchRequest {
                query: "Rust".to_owned(),
                from_date: Some("2026-01-01".to_owned()),
                to_date: Some("2026-08-29".to_owned()),
            }),
            "session-123",
        );

        assert_eq!(web["model"], MODEL);
        assert_eq!(web["store"], false);
        assert_eq!(
            web["include"],
            json!(["reasoning.encrypted_content", "no_inline_citations"])
        );
        assert_eq!(
            web["reasoning"],
            json!({"effort": REASONING_EFFORT, "summary": "concise"})
        );
        assert_eq!(web["prompt_cache_key"], "session-123");
        assert_eq!(web["tool_choice"], "required");
        assert_eq!(web["tools"][0]["type"], "web_search");
        assert_eq!(
            web["tools"][0]["filters"]["allowed_domains"][0],
            "rust-lang.org"
        );
        assert_eq!(x["model"], MODEL);
        assert_eq!(x["tools"][0]["type"], "x_search");
        assert_eq!(x["tools"][0]["from_date"], "2026-01-01");
        assert_eq!(x["tools"][0]["to_date"], "2026-08-29");
        assert_eq!(web["input"][0]["type"], "message");
        assert_eq!(web["input"][0]["role"], "user");
        assert_eq!(web["input"][0]["content"], "  Rust  ");

        let excluded = build_request(
            &ValidatedSearchRequest::Web(ValidatedWebSearchRequest {
                query: "Rust".to_owned(),
                allowed_domains: Vec::new(),
                excluded_domains: vec!["example.com".to_owned()],
            }),
            "session-123",
        );
        assert_eq!(
            excluded["tools"][0]["filters"]["excluded_domains"],
            json!(["example.com"])
        );
    }

    #[test]
    fn reuses_and_rotates_upstream_sessions() {
        let sessions = GrokUpstreamSessions::new(Duration::from_secs(60));
        let first = sessions.next_request_context();
        let second = sessions.next_request_context();
        assert_eq!(first.session_id, second.session_id);
        assert_ne!(first.request_id, second.request_id);
        assert!(Uuid::parse_str(&first.request_id).is_ok());
        assert!(Uuid::parse_str(&second.request_id).is_ok());
        assert_eq!(first.turn_index, 1);
        assert_eq!(second.turn_index, 2);

        let rotating = GrokUpstreamSessions::new(Duration::ZERO);
        let first = rotating.next_request_context();
        let second = rotating.next_request_context();
        assert_ne!(first.session_id, second.session_id);
        assert_eq!(first.turn_index, 1);
        assert_eq!(second.turn_index, 1);
    }

    #[tokio::test]
    async fn sends_required_headers_body_and_collects_sse() {
        let captured = Arc::new(Mutex::new(CapturedRequest::default()));
        let app = Router::new()
            .route("/v1/responses", post(responses_handler))
            .with_state(captured.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock listener");
        let address = listener.local_addr().expect("mock address");
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let client = GrokResponsesClient::new(format!("http://{address}/v1"), "agent-123");
        let credentials = GrokCredentials::from_json(&SecretString::from(
            r#"{"type":"xai","auth_kind":"oauth","access_token":"access-secret","upstream_user_id":"user-1","disabled":false}"#,
        ))
        .expect("credentials");
        let request = ValidatedSearchRequest::Web(ValidatedWebSearchRequest {
            query: "Rust".to_owned(),
            allowed_domains: Vec::new(),
            excluded_domains: Vec::new(),
        });

        let context = client.next_request_context();
        let output = client
            .execute(&credentials, &request, &context)
            .await
            .expect("Grok response");
        server.abort();

        assert_eq!(output.results.len(), 1);
        assert_eq!(output.results[0]["type"], "web_search_call");
        assert_eq!(output.results[0]["action"]["query"], "Rust");
        let captured = captured.lock().expect("capture lock");
        assert_eq!(captured.authorization, "Bearer access-secret");
        assert_eq!(captured.user_id, "user-1");
        assert_eq!(captured.model, MODEL);
        assert_eq!(captured.agent_id, "agent-123");
        assert_eq!(captured.conversation_id, captured.session_id);
        assert_eq!(captured.conversation_id, context.session_id);
        assert_eq!(captured.request_id, context.request_id);
        assert_ne!(captured.request_id, captured.session_id);
        assert_eq!(captured.turn_index, "1");
        assert_eq!(captured.body["model"], MODEL);
        assert_eq!(captured.body["prompt_cache_key"], context.session_id);
        assert_eq!(captured.body["tools"][0]["type"], "web_search");
        assert!(captured.body["tools"][0].get("filters").is_none());
        assert_eq!(captured.body["input"][0]["content"], "Rust");
    }

    #[tokio::test]
    async fn refreshes_and_retries_only_after_unauthorized() {
        let state = RetryState::default();
        let app = Router::new()
            .route("/discovery", get(discovery_handler))
            .route("/token", post(token_handler))
            .route("/v1/responses", post(retry_responses_handler))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock listener");
        let address = listener.local_addr().expect("mock address");
        let base_url = format!("http://{address}");
        *state.base_url.lock().expect("base URL lock") = base_url.clone();
        let server = tokio::spawn(async move { axum::serve(listener, app).await });

        let store = GrokCredentialStore::in_memory().await.expect("store");
        let credentials = GrokCredentials::from_json(&SecretString::from(format!(
            r#"{{"type":"xai","auth_kind":"oauth","access_token":"old-access","refresh_token":"old-refresh","upstream_user_id":"user-1","oidc_issuer":"{OIDC_ISSUER}","oidc_client_id":"{OIDC_CLIENT_ID}","expired":"2099-01-01T00:00:00Z","disabled":false}}"#
        )))
        .expect("credentials");
        store
            .save_authorization(&credentials)
            .await
            .expect("save credential");
        let refresh = GrokRefreshClient::for_test(format!("{base_url}/discovery"));
        let session = GrokCredentialSession::for_test(store.clone(), refresh)
            .await
            .expect("session");
        let provider = GrokSearchProvider::for_test(session, format!("{base_url}/v1"), "agent-123");

        let response = provider
            .search(ValidatedSearchRequest::Web(ValidatedWebSearchRequest {
                query: "Rust".to_owned(),
                allowed_domains: Vec::new(),
                excluded_domains: Vec::new(),
            }))
            .await
            .expect("retried search");
        server.abort();

        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0]["id"], "web-retry");
        assert_eq!(
            *state.authorizations.lock().expect("authorization lock"),
            ["Bearer old-access", "Bearer new-access"]
        );
        let stored = store
            .load()
            .await
            .expect("load credential")
            .expect("stored credential");
        assert_eq!(
            stored.credentials.access_token().expose_secret(),
            "new-access"
        );
    }
}

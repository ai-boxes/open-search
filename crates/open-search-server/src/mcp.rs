use open_search_core::{SearchError, SearchRequest, WebSearchRequest, XSearchRequest};
use open_search_runtime::SearchEngine;
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::{StreamableHttpServerConfig, StreamableHttpService},
};
use serde::Deserialize;
use serde_json::json;
use tracing::error;

#[derive(Clone)]
pub(crate) struct OpenSearchMcp {
    engine: SearchEngine,
    tool_router: ToolRouter<Self>,
}

impl OpenSearchMcp {
    fn new(engine: SearchEngine) -> Self {
        Self {
            engine,
            tool_router: Self::tool_router(),
        }
    }

    async fn run_search(&self, request: SearchRequest) -> Result<CallToolResult, ErrorData> {
        match self.engine.search(request).await {
            Ok(response) => match serde_json::to_value(response) {
                Ok(response) => Ok(CallToolResult::structured(response)),
                Err(error) => {
                    error!(%error, "failed to serialize MCP search result");
                    Err(ErrorData::internal_error(
                        "failed to serialize search result",
                        None,
                    ))
                }
            },
            Err(error) => Ok(search_error_result(error)),
        }
    }
}

#[tool_router]
impl OpenSearchMcp {
    #[tool(
        name = "grok_x_search",
        description = "Search X/Twitter through Grok. Use for posts, users, threads, and current discussions on X. Returns the Grok search result without post-processing."
    )]
    async fn grok_x_search(
        &self,
        Parameters(arguments): Parameters<GrokXSearchArguments>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run_search(SearchRequest::X(XSearchRequest {
            query: arguments.query,
            from_date: arguments.from_date,
            to_date: arguments.to_date,
        }))
        .await
    }

    #[tool(
        name = "grok_web_search",
        description = "Search the public web through Grok. Use for current web information and source URLs. Returns the Grok search result without post-processing."
    )]
    async fn grok_web_search(
        &self,
        Parameters(arguments): Parameters<GrokWebSearchArguments>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run_search(SearchRequest::Web(WebSearchRequest {
            query: arguments.query,
            allowed_domains: arguments.allowed_domains,
            excluded_domains: arguments.excluded_domains,
        }))
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for OpenSearchMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "open-search",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Use grok_x_search for X/Twitter sources and grok_web_search for public web sources.",
            )
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct GrokXSearchArguments {
    #[schemars(description = "The search query passed to Grok unchanged.")]
    query: String,
    #[serde(default)]
    #[schemars(description = "Optional inclusive content-date lower bound in YYYY-MM-DD format.")]
    from_date: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional exclusive content-date upper bound in YYYY-MM-DD format.")]
    to_date: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
struct GrokWebSearchArguments {
    #[schemars(description = "The search query passed to Grok unchanged.")]
    query: String,
    #[serde(default)]
    #[schemars(
        description = "Optional domain allowlist with at most 5 entries. Cannot be combined with excluded_domains."
    )]
    allowed_domains: Vec<String>,
    #[serde(default)]
    #[schemars(
        description = "Optional domain blocklist with at most 5 entries. Cannot be combined with allowed_domains."
    )]
    excluded_domains: Vec<String>,
}

fn search_error_result(error: SearchError) -> CallToolResult {
    let code = match &error {
        SearchError::InvalidRequest(_) => "invalid_request",
        SearchError::ProviderUnavailable(_) => "provider_unavailable",
        SearchError::CapabilityUnavailable(_) => "capability_unavailable",
        SearchError::NotAuthorized => "not_authorized",
        SearchError::ReauthorizationRequired => "reauthorization_required",
        SearchError::RateLimited => "rate_limited",
        SearchError::Timeout => "timeout",
        SearchError::UpstreamRejected(_) => "upstream_rejected",
        SearchError::UpstreamUnavailable(_) => "upstream_unavailable",
    };
    CallToolResult::structured_error(json!({
        "error": {
            "code": code,
            "message": error.to_string()
        }
    }))
}

pub(crate) fn mcp_service(
    engine: SearchEngine,
) -> StreamableHttpService<
    OpenSearchMcp,
    rmcp::transport::streamable_http_server::session::local::LocalSessionManager,
> {
    StreamableHttpService::new(
        move || Ok(OpenSearchMcp::new(engine.clone())),
        Default::default(),
        StreamableHttpServerConfig::default(),
    )
}

use std::path::PathBuf;

pub(crate) const DEFAULT_DATABASE_PATH: &str = "data/open-search.db";
const DATABASE_PATH_ENV: &str = "DATABASE_PATH";
const MCP_BEARER_TOKEN_ENV: &str = "MCP_BEARER_TOKEN";

pub(crate) fn database_path() -> PathBuf {
    std::env::var(DATABASE_PATH_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATABASE_PATH))
}

pub(crate) fn mcp_bearer_token() -> Option<String> {
    std::env::var(MCP_BEARER_TOKEN_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

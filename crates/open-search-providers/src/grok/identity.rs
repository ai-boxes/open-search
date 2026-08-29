use reqwest::RequestBuilder;

use super::contract::{CLIENT_IDENTIFIER, CLIENT_MODE, CLIENT_VERSION};

const TOKEN_AUTH_HEADER: &str = "X-XAI-Token-Auth";
const TOKEN_AUTH_VALUE: &str = "xai-grok-cli";

pub(crate) fn session_headers(request: RequestBuilder) -> RequestBuilder {
    request
        .header("x-grok-client-version", CLIENT_VERSION)
        .header("x-grok-client-mode", CLIENT_MODE)
        .header("x-grok-client-identifier", CLIENT_IDENTIFIER)
        .header(TOKEN_AUTH_HEADER, TOKEN_AUTH_VALUE)
        .header(reqwest::header::USER_AGENT, user_agent())
}

pub(crate) fn inference_headers(request: RequestBuilder) -> RequestBuilder {
    session_headers(request).header("x-authenticateresponse", "authenticate-response")
}

fn user_agent() -> String {
    format!(
        "grok-shell/{CLIENT_VERSION} ({}; {})",
        std::env::consts::OS,
        normalized_architecture()
    )
}

fn normalized_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "arm64" => "aarch64",
        architecture => architecture,
    }
}

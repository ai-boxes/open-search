use std::sync::Arc;

use open_search_core::{SearchError, SearchProvider, SearchRequest, SearchResponse};

#[derive(Clone)]
pub struct SearchEngine {
    provider: Arc<dyn SearchProvider>,
}

impl SearchEngine {
    pub fn new(provider: Arc<dyn SearchProvider>) -> Self {
        Self { provider }
    }

    pub async fn search(&self, request: SearchRequest) -> Result<SearchResponse, SearchError> {
        let request = request.validate()?;
        let capability = request.capability();
        if !self.provider.supports(capability) {
            return Err(SearchError::CapabilityUnavailable(capability));
        }

        self.provider.search(request).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use open_search_core::{
        SearchCapability, SearchError, SearchProvider, SearchRequest, SearchResponse,
        ValidatedSearchRequest, WebSearchRequest, XSearchRequest,
    };

    use super::SearchEngine;

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
            Ok(SearchResponse {
                results: vec![serde_json::json!({"type": kind, "query": query})],
            })
        }
    }

    struct WebOnlySearchProvider;

    #[async_trait]
    impl SearchProvider for WebOnlySearchProvider {
        fn supports(&self, capability: SearchCapability) -> bool {
            capability == SearchCapability::Web
        }

        async fn search(
            &self,
            _request: ValidatedSearchRequest,
        ) -> Result<SearchResponse, SearchError> {
            unreachable!("unsupported capability should not reach the provider")
        }
    }

    #[tokio::test]
    async fn executes_x_and_web_search_through_one_provider_contract() {
        let engine = SearchEngine::new(Arc::new(FakeSearchProvider));

        let x_response = engine
            .search(SearchRequest::X(XSearchRequest {
                query: "AI search".to_owned(),
                from_date: None,
                to_date: None,
            }))
            .await
            .expect("X search should succeed");
        let web_response = engine
            .search(SearchRequest::Web(WebSearchRequest {
                query: "AI search".to_owned(),
                allowed_domains: Vec::new(),
                excluded_domains: Vec::new(),
            }))
            .await
            .expect("Web search should succeed");

        assert_eq!(x_response.results[0]["type"], "x");
        assert_eq!(x_response.results[0]["query"], "AI search");
        assert_eq!(web_response.results[0]["type"], "web");
    }

    #[tokio::test]
    async fn rejects_unsupported_capability_before_calling_provider() {
        let engine = SearchEngine::new(Arc::new(WebOnlySearchProvider));
        let error = engine
            .search(SearchRequest::X(XSearchRequest {
                query: "AI search".to_owned(),
                from_date: None,
                to_date: None,
            }))
            .await
            .expect_err("unsupported search should fail");

        assert_eq!(
            error,
            SearchError::CapabilityUnavailable(SearchCapability::X)
        );
    }
}

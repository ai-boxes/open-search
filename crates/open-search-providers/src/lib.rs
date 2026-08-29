use async_trait::async_trait;
use open_search_core::{
    SearchCapability, SearchError, SearchProvider, SearchResponse, ValidatedSearchRequest,
};

pub mod grok;

#[derive(Debug, Default)]
pub struct UnconfiguredSearchProvider;

#[async_trait]
impl SearchProvider for UnconfiguredSearchProvider {
    fn supports(&self, capability: SearchCapability) -> bool {
        matches!(capability, SearchCapability::X | SearchCapability::Web)
    }

    async fn search(
        &self,
        _request: ValidatedSearchRequest,
    ) -> Result<SearchResponse, SearchError> {
        Err(SearchError::ProviderUnavailable(
            "no search provider has been configured".to_owned(),
        ))
    }
}

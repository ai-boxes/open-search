use async_trait::async_trait;
use open_search_core::{
    AnswerProvider, AnswerRequest, GeneratedAnswer, SearchError, SearchHit, SearchProvider,
    ValidatedSearchRequest,
};

#[derive(Debug, Default)]
pub struct UnconfiguredSearchProvider;

#[async_trait]
impl SearchProvider for UnconfiguredSearchProvider {
    async fn search(
        &self,
        _request: &ValidatedSearchRequest,
    ) -> Result<Vec<SearchHit>, SearchError> {
        Err(SearchError::SearchProviderUnavailable(
            "no search provider has been configured".to_owned(),
        ))
    }
}

#[derive(Debug, Default)]
pub struct UnconfiguredAnswerProvider;

#[async_trait]
impl AnswerProvider for UnconfiguredAnswerProvider {
    async fn answer(&self, _request: AnswerRequest) -> Result<GeneratedAnswer, SearchError> {
        Err(SearchError::AnswerProviderUnavailable(
            "no answer provider has been configured".to_owned(),
        ))
    }
}

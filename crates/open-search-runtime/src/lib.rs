use std::sync::Arc;

use open_search_core::{
    AnswerProvider, AnswerRequest, SearchError, SearchProvider, SearchRequest, SearchResponse,
};

#[derive(Clone)]
pub struct SearchEngine {
    search_provider: Arc<dyn SearchProvider>,
    answer_provider: Arc<dyn AnswerProvider>,
}

impl SearchEngine {
    pub fn new(
        search_provider: Arc<dyn SearchProvider>,
        answer_provider: Arc<dyn AnswerProvider>,
    ) -> Self {
        Self {
            search_provider,
            answer_provider,
        }
    }

    pub async fn search(&self, request: SearchRequest) -> Result<SearchResponse, SearchError> {
        let request = request.validate()?;
        let sources = self.search_provider.search(&request).await?;
        let answer = self
            .answer_provider
            .answer(AnswerRequest {
                query: request.query,
                sources: sources.clone(),
            })
            .await?;

        Ok(SearchResponse {
            answer: answer.text,
            sources,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use open_search_core::{
        AnswerProvider, AnswerRequest, GeneratedAnswer, SearchError, SearchHit, SearchProvider,
        SearchRequest, ValidatedSearchRequest,
    };

    use super::SearchEngine;

    struct FakeSearchProvider;

    #[async_trait]
    impl SearchProvider for FakeSearchProvider {
        async fn search(
            &self,
            request: &ValidatedSearchRequest,
        ) -> Result<Vec<SearchHit>, SearchError> {
            Ok(vec![SearchHit {
                title: request.query.clone(),
                url: "https://example.com/result".to_owned(),
                snippet: "Evidence".to_owned(),
                score: Some(1.0),
            }])
        }
    }

    struct FakeAnswerProvider;

    #[async_trait]
    impl AnswerProvider for FakeAnswerProvider {
        async fn answer(&self, request: AnswerRequest) -> Result<GeneratedAnswer, SearchError> {
            Ok(GeneratedAnswer {
                text: format!("{} source(s) for {}", request.sources.len(), request.query),
            })
        }
    }

    #[tokio::test]
    async fn orchestrates_retrieval_before_answer_generation() {
        let engine = SearchEngine::new(Arc::new(FakeSearchProvider), Arc::new(FakeAnswerProvider));
        let response = engine
            .search(SearchRequest {
                query: "AI search".to_owned(),
                max_results: Some(5),
            })
            .await
            .expect("pipeline should succeed");

        assert_eq!(response.answer, "1 source(s) for AI search");
        assert_eq!(response.sources.len(), 1);
    }
}

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use open_search_core::SearchError;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info, warn};

use super::{
    CredentialWriteOutcome, GrokCredentialStore, GrokCredentials, GrokRefreshClient,
    GrokRefreshErrorKind, StoredGrokCredential,
};

const REFRESH_SKEW_SECONDS: i64 = 60;

#[derive(Clone)]
pub struct GrokCredentialSession {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    store: GrokCredentialStore,
    refresh: GrokRefreshClient,
    current: RwLock<Option<StoredGrokCredential>>,
    refresh_lock: Mutex<()>,
}

impl GrokCredentialSession {
    pub async fn load(store: GrokCredentialStore) -> Result<Self, SearchError> {
        let current = store.load().await.map_err(store_error)?;
        Ok(Self::from_parts(store, GrokRefreshClient::new(), current))
    }

    fn from_parts(
        store: GrokCredentialStore,
        refresh: GrokRefreshClient,
        current: Option<StoredGrokCredential>,
    ) -> Self {
        Self {
            inner: Arc::new(SessionInner {
                store,
                refresh,
                current: RwLock::new(current),
                refresh_lock: Mutex::new(()),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) async fn for_test(
        store: GrokCredentialStore,
        refresh: GrokRefreshClient,
    ) -> Result<Self, SearchError> {
        let current = store.load().await.map_err(store_error)?;
        Ok(Self::from_parts(store, refresh, current))
    }

    pub(crate) async fn credentials(&self) -> Result<GrokCredentials, SearchError> {
        self.ensure_credentials(false).await
    }

    pub async fn has_credentials(&self) -> bool {
        self.inner.current.read().await.is_some()
    }

    pub(crate) async fn refresh_after_unauthorized(&self) -> Result<GrokCredentials, SearchError> {
        self.ensure_credentials(true).await
    }

    async fn ensure_credentials(
        &self,
        force_refresh: bool,
    ) -> Result<GrokCredentials, SearchError> {
        if !force_refresh && let Some(credentials) = self.fresh_credentials().await? {
            return Ok(credentials);
        }

        let _guard = self.inner.refresh_lock.lock().await;
        self.reload_latest().await?;
        if !force_refresh && let Some(credentials) = self.fresh_credentials().await? {
            return Ok(credentials);
        }

        let current = self
            .inner
            .current
            .read()
            .await
            .as_ref()
            .map(|stored| (stored.credentials.clone(), stored.revision))
            .ok_or(SearchError::NotAuthorized)?;
        let trigger = if force_refresh {
            "upstream_unauthorized"
        } else {
            "expiry"
        };
        let refreshed = match self.inner.refresh.refresh(&current.0).await {
            Ok(refreshed) => refreshed,
            Err(error) => {
                warn!(
                    provider = "grok",
                    trigger,
                    error = refresh_error_code(error.kind()),
                    "credential refresh failed"
                );
                return Err(refresh_error(error));
            }
        };

        match self
            .inner
            .store
            .compare_and_swap(current.1, &refreshed)
            .await
            .map_err(store_error)?
        {
            CredentialWriteOutcome::Updated { revision } => {
                *self.inner.current.write().await = Some(StoredGrokCredential {
                    credentials: refreshed.clone(),
                    revision,
                });
                info!(provider = "grok", trigger, "credential refresh completed");
                Ok(refreshed)
            }
            CredentialWriteOutcome::Conflict => {
                let latest = self
                    .inner
                    .store
                    .load()
                    .await
                    .map_err(store_error)?
                    .ok_or(SearchError::NotAuthorized)?;
                let credentials = latest.credentials.clone();
                *self.inner.current.write().await = Some(latest);
                debug!(
                    provider = "grok",
                    trigger, "credential refresh result superseded by newer credential"
                );
                Ok(credentials)
            }
        }
    }

    async fn reload_latest(&self) -> Result<(), SearchError> {
        let current = self.inner.store.load().await.map_err(store_error)?;
        *self.inner.current.write().await = current;
        Ok(())
    }

    async fn fresh_credentials(&self) -> Result<Option<GrokCredentials>, SearchError> {
        let current = self.inner.current.read().await;
        let Some(stored) = current.as_ref() else {
            return Ok(None);
        };
        let expires_at = stored
            .credentials
            .expires_at()
            .map_err(|_| SearchError::ReauthorizationRequired)?;
        let fresh = expires_at
            .is_none_or(|expires_at| expires_at > unix_timestamp() + REFRESH_SKEW_SECONDS);
        Ok(fresh.then(|| stored.credentials.clone()))
    }
}

fn refresh_error(error: super::GrokRefreshError) -> SearchError {
    match error.kind() {
        GrokRefreshErrorKind::ReauthorizationRequired => SearchError::ReauthorizationRequired,
        GrokRefreshErrorKind::Transient => {
            SearchError::ProviderUnavailable("Grok OAuth refresh temporarily failed".to_owned())
        }
        GrokRefreshErrorKind::Internal => {
            SearchError::ProviderUnavailable("Grok OAuth refresh could not be completed".to_owned())
        }
    }
}

fn refresh_error_code(kind: GrokRefreshErrorKind) -> &'static str {
    match kind {
        GrokRefreshErrorKind::ReauthorizationRequired => "reauthorization_required",
        GrokRefreshErrorKind::Transient => "transient",
        GrokRefreshErrorKind::Internal => "internal",
    }
}

fn store_error(error: super::GrokStoreError) -> SearchError {
    error!(provider = "grok", %error, "credential storage failed");
    SearchError::ProviderUnavailable("Grok credential storage is unavailable".to_owned())
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

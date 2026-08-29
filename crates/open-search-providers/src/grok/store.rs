use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use secrecy::{ExposeSecret, SecretString};
use sqlx::{
    ConnectOptions, Row, SqlitePool,
    migrate::Migrator,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use thiserror::Error;

use super::credentials::{GrokAuthError, GrokCredentials};

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

const ACCOUNT_ID: &str = "default";
const CREDENTIAL_FORMAT_VERSION: i64 = 1;

#[derive(Clone)]
pub struct GrokCredentialStore {
    pool: SqlitePool,
}

#[derive(Debug)]
pub struct StoredGrokCredential {
    pub credentials: GrokCredentials,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialWriteOutcome {
    Updated { revision: u64 },
    Conflict,
}

impl GrokCredentialStore {
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, GrokStoreError> {
        let path = path.as_ref();
        prepare_data_directory(path)?;
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true)
            .disable_statement_logging();
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(GrokStoreError::database)?;
        MIGRATOR
            .run(&pool)
            .await
            .map_err(GrokStoreError::migration)?;
        restrict_sqlite_permissions(path)?;

        Ok(Self { pool })
    }

    #[cfg(test)]
    pub(crate) async fn in_memory() -> Result<Self, GrokStoreError> {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(GrokStoreError::database)?;
        MIGRATOR
            .run(&pool)
            .await
            .map_err(GrokStoreError::migration)?;
        Ok(Self { pool })
    }

    pub async fn load(&self) -> Result<Option<StoredGrokCredential>, GrokStoreError> {
        let row = sqlx::query(
            r#"
            SELECT revision, format_version, credential_json
            FROM grok_credentials
            WHERE account_id = ?
            "#,
        )
        .bind(ACCOUNT_ID)
        .fetch_optional(&self.pool)
        .await
        .map_err(GrokStoreError::database)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let revision: i64 = row.try_get("revision").map_err(GrokStoreError::database)?;
        let format_version: i64 = row
            .try_get("format_version")
            .map_err(GrokStoreError::database)?;
        if format_version != CREDENTIAL_FORMAT_VERSION {
            return Err(GrokStoreError::UnsupportedCredentialFormat(format_version));
        }
        let credential_json: String = row
            .try_get("credential_json")
            .map_err(GrokStoreError::database)?;
        let credentials = GrokCredentials::from_json(&SecretString::from(credential_json))
            .map_err(GrokStoreError::credential)?;
        if !credentials.has_supported_refresh_provenance() {
            return Err(GrokStoreError::UnsupportedOAuthProvenance);
        }
        let revision = u64::try_from(revision).map_err(|_| GrokStoreError::InvalidRevision)?;
        Ok(Some(StoredGrokCredential {
            credentials,
            revision,
        }))
    }

    pub async fn load_or_insert_metadata(
        &self,
        key: &str,
        candidate: &str,
    ) -> Result<String, GrokStoreError> {
        sqlx::query(
            r#"
            INSERT INTO application_metadata (key, value, updated_at)
            VALUES (?, ?, ?)
            ON CONFLICT (key) DO NOTHING
            "#,
        )
        .bind(key)
        .bind(candidate)
        .bind(unix_timestamp())
        .execute(&self.pool)
        .await
        .map_err(GrokStoreError::database)?;

        sqlx::query_scalar(
            r#"
            SELECT value
            FROM application_metadata
            WHERE key = ?
            "#,
        )
        .bind(key)
        .fetch_one(&self.pool)
        .await
        .map_err(GrokStoreError::database)
    }

    pub async fn save_authorization(
        &self,
        credentials: &GrokCredentials,
    ) -> Result<u64, GrokStoreError> {
        validate_storable(credentials)?;
        let credential_json = credentials.to_json().map_err(GrokStoreError::credential)?;
        let expires_at = credentials
            .expires_at()
            .map_err(GrokStoreError::credential)?;
        let last_refreshed_at = credentials
            .last_refreshed_at()
            .map_err(GrokStoreError::credential)?;
        let revision: i64 = sqlx::query_scalar(
            r#"
            INSERT INTO grok_credentials (
                account_id,
                revision,
                format_version,
                credential_json,
                expires_at,
                last_refreshed_at,
                updated_at
            ) VALUES (?, 0, ?, ?, ?, ?, ?)
            ON CONFLICT (account_id) DO UPDATE SET
                revision = grok_credentials.revision + 1,
                format_version = excluded.format_version,
                credential_json = excluded.credential_json,
                expires_at = excluded.expires_at,
                last_refreshed_at = excluded.last_refreshed_at,
                updated_at = excluded.updated_at
            RETURNING revision
            "#,
        )
        .bind(ACCOUNT_ID)
        .bind(CREDENTIAL_FORMAT_VERSION)
        .bind(credential_json.expose_secret())
        .bind(expires_at)
        .bind(last_refreshed_at)
        .bind(unix_timestamp())
        .fetch_one(&self.pool)
        .await
        .map_err(GrokStoreError::database)?;
        u64::try_from(revision).map_err(|_| GrokStoreError::InvalidRevision)
    }

    pub async fn compare_and_swap(
        &self,
        expected_revision: u64,
        credentials: &GrokCredentials,
    ) -> Result<CredentialWriteOutcome, GrokStoreError> {
        validate_storable(credentials)?;
        let expected_revision =
            i64::try_from(expected_revision).map_err(|_| GrokStoreError::InvalidRevision)?;
        let credential_json = credentials.to_json().map_err(GrokStoreError::credential)?;
        let expires_at = credentials
            .expires_at()
            .map_err(GrokStoreError::credential)?;
        let last_refreshed_at = credentials
            .last_refreshed_at()
            .map_err(GrokStoreError::credential)?;
        let result = sqlx::query(
            r#"
            UPDATE grok_credentials
            SET
                revision = revision + 1,
                credential_json = ?,
                expires_at = ?,
                last_refreshed_at = ?,
                updated_at = ?
            WHERE account_id = ? AND revision = ?
            "#,
        )
        .bind(credential_json.expose_secret())
        .bind(expires_at)
        .bind(last_refreshed_at)
        .bind(unix_timestamp())
        .bind(ACCOUNT_ID)
        .bind(expected_revision)
        .execute(&self.pool)
        .await
        .map_err(GrokStoreError::database)?;
        if result.rows_affected() == 0 {
            return Ok(CredentialWriteOutcome::Conflict);
        }
        let revision = expected_revision
            .checked_add(1)
            .and_then(|revision| u64::try_from(revision).ok())
            .ok_or(GrokStoreError::InvalidRevision)?;
        Ok(CredentialWriteOutcome::Updated { revision })
    }
}

fn validate_storable(credentials: &GrokCredentials) -> Result<(), GrokStoreError> {
    if !credentials.has_supported_refresh_provenance() {
        return Err(GrokStoreError::UnsupportedOAuthProvenance);
    }
    Ok(())
}

fn prepare_data_directory(path: &Path) -> Result<(), GrokStoreError> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    std::fs::create_dir_all(parent).map_err(GrokStoreError::io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(GrokStoreError::io)?;
    }
    Ok(())
}

fn restrict_sqlite_permissions(path: &Path) -> Result<(), GrokStoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        for file in sqlite_files(path) {
            match std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(GrokStoreError::io(error)),
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sqlite_files(path: &Path) -> [std::path::PathBuf; 3] {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let mut shared_memory = path.as_os_str().to_owned();
    shared_memory.push("-shm");
    [path.to_path_buf(), wal.into(), shared_memory.into()]
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

#[derive(Debug, Error)]
pub enum GrokStoreError {
    #[error("failed to access Grok credential database")]
    Database(#[source] sqlx::Error),
    #[error("failed to migrate Grok credential database")]
    Migration(#[source] sqlx::migrate::MigrateError),
    #[error("failed to prepare Grok credential storage")]
    Io(#[source] std::io::Error),
    #[error("unsupported Grok credential format version {0}")]
    UnsupportedCredentialFormat(i64),
    #[error("stored Grok credential revision is invalid")]
    InvalidRevision,
    #[error("Grok credential has unsupported OAuth provenance")]
    UnsupportedOAuthProvenance,
    #[error("stored Grok credential is invalid")]
    Credential(#[source] GrokAuthError),
}

impl GrokStoreError {
    fn database(error: sqlx::Error) -> Self {
        Self::Database(error)
    }

    fn migration(error: sqlx::migrate::MigrateError) -> Self {
        Self::Migration(error)
    }

    fn io(error: std::io::Error) -> Self {
        Self::Io(error)
    }

    fn credential(error: GrokAuthError) -> Self {
        Self::Credential(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grok::contract::{OIDC_CLIENT_ID, OIDC_ISSUER};

    fn credential(access_token: &str) -> GrokCredentials {
        GrokCredentials::from_json(&SecretString::from(format!(
            r#"{{"type":"xai","auth_kind":"oauth","access_token":"{access_token}","refresh_token":"refresh","oidc_issuer":"{OIDC_ISSUER}","oidc_client_id":"{OIDC_CLIENT_ID}","disabled":false}}"#
        )))
        .expect("credential")
    }

    #[tokio::test]
    async fn stores_loads_and_compare_and_swaps_single_credential() {
        let store = GrokCredentialStore::in_memory().await.expect("store");
        let initial = credential("secret-access");
        let revision = store
            .save_authorization(&initial)
            .await
            .expect("save credential");
        assert_eq!(revision, 0);

        let credential_json: String = sqlx::query_scalar(
            "SELECT credential_json FROM grok_credentials WHERE account_id = 'default'",
        )
        .fetch_one(&store.pool)
        .await
        .expect("credential JSON");
        let document: serde_json::Value =
            serde_json::from_str(&credential_json).expect("credential document");
        assert_eq!(document["access_token"], "secret-access");

        let stored = store.load().await.expect("load").expect("credential");
        assert_eq!(stored.revision, 0);

        let replacement = credential("replacement-access");
        assert_eq!(
            store
                .compare_and_swap(1, &replacement)
                .await
                .expect("conflict"),
            CredentialWriteOutcome::Conflict
        );
        assert_eq!(
            store
                .compare_and_swap(0, &replacement)
                .await
                .expect("update"),
            CredentialWriteOutcome::Updated { revision: 1 }
        );
        let stored = store.load().await.expect("load").expect("credential");
        let document: serde_json::Value = serde_json::from_str(
            stored
                .credentials
                .to_json()
                .expect("credential JSON")
                .expose_secret(),
        )
        .expect("credential document");
        assert_eq!(document["access_token"], "replacement-access");
    }

    #[tokio::test]
    async fn persists_credential_across_database_reopen() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("open-search.db");
        let store = GrokCredentialStore::connect(&database)
            .await
            .expect("store");
        store
            .save_authorization(&credential("persisted-access"))
            .await
            .expect("save credential");
        store.pool.close().await;

        let reopened = GrokCredentialStore::connect(&database)
            .await
            .expect("reopened store");
        let stored = reopened.load().await.expect("load").expect("credential");
        assert_eq!(stored.revision, 0);
    }

    #[tokio::test]
    async fn application_metadata_keeps_the_first_value_across_reopen() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("open-search.db");
        let store = GrokCredentialStore::connect(&database)
            .await
            .expect("store");
        let first = store
            .load_or_insert_metadata("grok.agent_id", "agent-1")
            .await
            .expect("first value");
        let existing = store
            .load_or_insert_metadata("grok.agent_id", "agent-2")
            .await
            .expect("existing value");
        assert_eq!(first, "agent-1");
        assert_eq!(existing, "agent-1");
        store.pool.close().await;

        let reopened = GrokCredentialStore::connect(&database)
            .await
            .expect("reopened store");
        let persisted = reopened
            .load_or_insert_metadata("grok.agent_id", "agent-3")
            .await
            .expect("persisted value");
        assert_eq!(persisted, "agent-1");
    }

    #[tokio::test]
    async fn rejects_credentials_without_refresh_provenance() {
        let store = GrokCredentialStore::in_memory().await.expect("store");
        let credentials = GrokCredentials::from_json(&SecretString::from(
            r#"{"type":"xai","auth_kind":"oauth","access_token":"access-only"}"#,
        ))
        .expect("access credential");

        let error = store
            .save_authorization(&credentials)
            .await
            .expect_err("unsupported provenance should fail");

        assert!(matches!(error, GrokStoreError::UnsupportedOAuthProvenance));
    }
}

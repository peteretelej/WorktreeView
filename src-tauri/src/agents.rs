use crate::store::now_millis;
use crate::CommandError;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
const MAX_TOKEN_NAME_CHARS: usize = 200;
const DEFAULT_TOKEN_NAME: &str = "agent";
// Secrets carry this recognizable prefix so a pasted string can be
// identified as a WorktreeView token in agent configs; it is part of the
// secret and covered by the stored hash. Kept separator-free so a
// double-click selects the whole token in editors and terminals.
const TOKEN_PREFIX: &str = "wv";

// The authenticated caller resolved from a bearer secret: only the token row
// id and its display name leave this module, never the hash or secret.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentIdentity {
    pub(crate) token_id: i64,
    pub(crate) name: String,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct AgentToken {
    pub id: i64,
    pub name: String,
    pub is_default: bool,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

// The secret exists only in this value: shown once at creation, never stored
// or logged; the store keeps only its SHA-256 hex hash. Debug redacts the
// secret wherever the creating face (Settings IPC or the admin API) logs;
// serialization keeps it so the creating client can show it once.
#[derive(Serialize)]
pub struct CreatedAgentToken {
    pub token: AgentToken,
    pub secret: String,
}

impl std::fmt::Debug for CreatedAgentToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedAgentToken")
            .field("token", &self.token)
            .field("secret", &"[redacted]")
            .finish()
    }
}

pub(crate) fn generate_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("Could not generate the endpoint token: {error}"))?;
    let mut token = String::with_capacity(TOKEN_PREFIX.len() + 64);
    token.push_str(TOKEN_PREFIX);
    for byte in bytes {
        token.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
        token.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
    }
    Ok(token)
}

pub(crate) fn hash_secret(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
        hex.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
    }
    hex
}

fn agent_token_from_row(row: &sqlx::sqlite::SqliteRow) -> sqlx::Result<AgentToken> {
    Ok(AgentToken {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        is_default: row.try_get::<i64, _>("is_default")? != 0,
        created_at: row.try_get("created_at")?,
        last_used_at: row.try_get("last_used_at")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}

pub(crate) async fn create_agent_token_in_pool(
    pool: &SqlitePool,
    name: &str,
) -> Result<CreatedAgentToken, CommandError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CommandError::new(
            "invalid_agent_token",
            "A token needs a non-empty name.",
        ));
    }
    if name.chars().count() > MAX_TOKEN_NAME_CHARS {
        return Err(CommandError::new(
            "invalid_agent_token",
            format!("The token name exceeds {MAX_TOKEN_NAME_CHARS} characters."),
        ));
    }
    // Names are the review-request claim vocabulary, so live tokens must
    // stay uniquely named; revoked names may be reused.
    let taken: Option<i64> =
        sqlx::query_scalar("SELECT id FROM agent_tokens WHERE name = ? AND revoked_at IS NULL")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    if taken.is_some() {
        return Err(CommandError::new(
            "invalid_agent_token",
            format!("A token named {name} already exists; token names must be unique."),
        ));
    }
    let secret = generate_token()
        .map_err(|error| CommandError::new("persistence", error))?;
    let hash = hash_secret(&secret);
    let created_at = now_millis();
    let result = sqlx::query(
        "INSERT INTO agent_tokens (name, secret_hash, is_default, created_at) \
         VALUES (?, ?, 0, ?)",
    )
    .bind(name)
    .bind(hash)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(CreatedAgentToken {
        token: AgentToken {
            id: result.last_insert_rowid(),
            name: name.into(),
            is_default: false,
            created_at,
            last_used_at: None,
            revoked_at: None,
        },
        secret,
    })
}

pub(crate) async fn list_agent_tokens_in_pool(
    pool: &SqlitePool,
) -> Result<Vec<AgentToken>, CommandError> {
    let rows = sqlx::query(
        "SELECT id, name, is_default, created_at, last_used_at, revoked_at \
         FROM agent_tokens ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(agent_token_from_row)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

// Deleting a token refuses it immediately: the row is gone, so the secret
// stops authenticating and the comments it authored stay as history with no
// owning agent (their author_token_id reference is set to null). There is no
// un-delete; mint a new token instead.
pub(crate) async fn delete_agent_token_in_pool(pool: &SqlitePool, id: i64) -> Result<(), CommandError> {
    let row: Option<bool> = sqlx::query_scalar("SELECT is_default FROM agent_tokens WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    match row {
        None => Err(CommandError::new(
            "invalid_agent_token",
            "The token does not exist.",
        )),
        Some(true) => Err(CommandError::new(
            "default_token_locked",
            "The built-in default token cannot be deleted; it renews at the next app start.",
        )),
        Some(false) => {
            sqlx::query("DELETE FROM agent_tokens WHERE id = ?")
                .bind(id)
                .execute(pool)
                .await?;
            Ok(())
        }
    }
}

// Hashes the presented secret and matches a non-revoked row; database
// failures fail closed. The presented secret never reaches storage.
pub(crate) async fn authenticate_token_in_pool(pool: &SqlitePool, secret: &str) -> Option<AgentIdentity> {
    let hash = hash_secret(secret);
    let (token_id, name): (i64, String) = sqlx::query_as(
        "SELECT id, name FROM agent_tokens WHERE secret_hash = ? AND revoked_at IS NULL",
    )
    .bind(hash)
    .fetch_optional(pool)
    .await
    .ok()??;
    // Activity recording is best-effort; the authentication already decided.
    let _ = sqlx::query("UPDATE agent_tokens SET last_used_at = ? WHERE id = ?")
        .bind(now_millis())
        .bind(token_id)
        .execute(pool)
        .await;
    Some(AgentIdentity { token_id, name })
}

// The listener's start provisioning: deletes the previous default (plus
// any revoked rows left by older builds) and inserts a fresh one in one
// transaction, so the table never accumulates dead defaults. The returned
// secret goes only into the config file written by the same startup path.
pub(crate) async fn provision_default_token_in_pool(pool: &SqlitePool) -> Result<String, CommandError> {
    let secret = generate_token()
        .map_err(|error| CommandError::new("persistence", error))?;
    let hash = hash_secret(&secret);
    let now = now_millis();
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM agent_tokens WHERE revoked_at IS NOT NULL")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM agent_tokens WHERE is_default = 1")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO agent_tokens (name, secret_hash, is_default, created_at) \
         VALUES (?, ?, 1, ?)",
    )
    .bind(DEFAULT_TOKEN_NAME)
    .bind(hash)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::test_pool;

    #[test]
    fn token_has_the_wv_prefix_lowercase_hex_body_and_is_unique_per_call() {
        let first = generate_token().unwrap();
        let second = generate_token().unwrap();
        assert!(first.starts_with(TOKEN_PREFIX));
        assert_eq!(first.len(), TOKEN_PREFIX.len() + 64);
        let body = &first[TOKEN_PREFIX.len()..];
        assert!(body.chars().all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase()));
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn created_tokens_list_without_their_secret_and_delete() {
        let pool = test_pool().await;
        let created = create_agent_token_in_pool(&pool, "codex").await.unwrap();
        assert!(!created.secret.is_empty());
        assert_ne!(created.secret, hash_secret(&created.secret));
        let listed = list_agent_tokens_in_pool(&pool).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.token.id);
        assert_eq!(listed[0].name, "codex");
        assert!(!listed[0].is_default);
        assert_eq!(listed[0].revoked_at, None);
        assert!(delete_agent_token_in_pool(&pool, listed[0].id).await.is_ok());
        assert!(list_agent_tokens_in_pool(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn token_names_are_validated() {
        let pool = test_pool().await;
        assert_eq!(
            create_agent_token_in_pool(&pool, "   ").await.unwrap_err().code,
            "invalid_agent_token"
        );
        let long = "x".repeat(MAX_TOKEN_NAME_CHARS + 1);
        assert_eq!(
            create_agent_token_in_pool(&pool, &long).await.unwrap_err().code,
            "invalid_agent_token"
        );
        assert!(list_agent_tokens_in_pool(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn authenticating_bumps_last_used_and_refuses_deleted_or_unknown() {
        let pool = test_pool().await;
        let created = create_agent_token_in_pool(&pool, "agent").await.unwrap();
        let identity = authenticate_token_in_pool(&pool, &created.secret).await.unwrap();
        assert_eq!(identity.token_id, created.token.id);
        assert_eq!(identity.name, "agent");
        let after_use: Option<i64> =
            sqlx::query_scalar("SELECT last_used_at FROM agent_tokens WHERE id = ?")
                .bind(created.token.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(after_use.is_some());

        assert!(authenticate_token_in_pool(&pool, "not-a-real-secret").await.is_none());
        delete_agent_token_in_pool(&pool, created.token.id).await.unwrap();
        assert!(authenticate_token_in_pool(&pool, &created.secret).await.is_none());
    }

    #[tokio::test]
    async fn provisioning_replaces_the_default_and_locks_it_against_deletion() {
        let pool = test_pool().await;
        let first = provision_default_token_in_pool(&pool).await.unwrap();
        let default_row = list_agent_tokens_in_pool(&pool).await.unwrap().remove(0);
        assert!(default_row.is_default);
        assert_eq!(default_row.name, DEFAULT_TOKEN_NAME);

        // A second boot deletes the previous default and inserts a fresh
        // one, so dead defaults never accumulate.
        let second = provision_default_token_in_pool(&pool).await.unwrap();
        assert_ne!(first, second);
        assert!(authenticate_token_in_pool(&pool, &first).await.is_none());
        assert!(authenticate_token_in_pool(&pool, &second).await.is_some());
        let rows = list_agent_tokens_in_pool(&pool).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_default);

        // The live default refuses deletion; a named token does not.
        assert_eq!(
            delete_agent_token_in_pool(&pool, rows[0].id).await.unwrap_err().code,
            "default_token_locked"
        );
        let named = create_agent_token_in_pool(&pool, "named").await.unwrap();
        assert!(delete_agent_token_in_pool(&pool, named.token.id).await.is_ok());
    }

    // Revoked rows left behind by older builds are dead credentials; the
    // first provisioning after an upgrade sweeps them.
    #[tokio::test]
    async fn provisioning_sweeps_legacy_revoked_rows() {
        let pool = test_pool().await;
        let named = create_agent_token_in_pool(&pool, "legacy").await.unwrap();
        sqlx::query("UPDATE agent_tokens SET revoked_at = 1 WHERE id = ?")
            .bind(named.token.id)
            .execute(&pool)
            .await
            .unwrap();
        provision_default_token_in_pool(&pool).await.unwrap();
        let rows = list_agent_tokens_in_pool(&pool).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_default);
    }

    // Named tokens persist across the default rotations on listener starts: a fresh
    // boot's provisioning never touches them, and their secrets keep working.
    #[tokio::test]
    async fn named_tokens_survive_boots() {
        let pool = test_pool().await;
        let named = create_agent_token_in_pool(&pool, "claude").await.unwrap();
        provision_default_token_in_pool(&pool).await.unwrap();
        provision_default_token_in_pool(&pool).await.unwrap();
        let identity = authenticate_token_in_pool(&pool, &named.secret).await.unwrap();
        assert_eq!(identity.token_id, named.token.id);
        let rows = list_agent_tokens_in_pool(&pool).await.unwrap();
        assert!(rows.iter().any(|token| token.id == named.token.id && token.revoked_at.is_none()));
    }

    #[tokio::test]
    async fn deleting_an_unknown_token_reports_it() {
        let pool = test_pool().await;
        assert_eq!(
            delete_agent_token_in_pool(&pool, 404).await.unwrap_err().code,
            "invalid_agent_token"
        );
    }
}

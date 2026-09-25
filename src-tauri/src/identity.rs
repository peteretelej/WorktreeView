//! Named human accounts and their bearer tokens: the server's identity
//! substrate. Secrets reuse the agent token mechanism (a `wv`-prefixed
//! secret generated once, only its SHA-256 hex hash persisted); exactly
//! the first created user is admin, and refusing a second one is the
//! entire no-open-registration gate.

use crate::agents::{generate_token, hash_secret};
use crate::store::now_millis;
use crate::CommandError;
use serde::Serialize;
use sqlx::{Row, SqlitePool};

// The same bound the users.name CHECK enforces; validation keeps stored
// names inside what the column admits.
const MAX_USER_NAME_CHARS: usize = 200;

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub is_admin: bool,
    pub created_at: i64,
}

// The secret exists only in this value: printed once by `create-admin`,
// never stored or logged; the store keeps only its SHA-256 hex hash.
#[derive(Debug, Serialize)]
pub struct CreatedAdmin {
    pub user: User,
    pub secret: String,
}

#[derive(Debug, Serialize)]
pub struct UserToken {
    pub id: i64,
    pub user_id: i64,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

// The plaintext secret is returned once and never stored.
#[derive(Debug, Serialize)]
pub struct CreatedUserToken {
    pub token: UserToken,
    pub secret: String,
}

fn user_from_row(row: &sqlx::sqlite::SqliteRow) -> sqlx::Result<User> {
    Ok(User {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        is_admin: row.try_get::<i64, _>("is_admin")? != 0,
        created_at: row.try_get("created_at")?,
    })
}

fn validate_name(name: &str) -> Result<&str, CommandError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CommandError::new(
            "invalid_user",
            "A user needs a non-empty name.",
        ));
    }
    if name.chars().count() > MAX_USER_NAME_CHARS {
        return Err(CommandError::new(
            "invalid_user",
            format!("The user name exceeds {MAX_USER_NAME_CHARS} characters."),
        ));
    }
    Ok(name)
}

// Bootstraps the first admin with their initial token in one transaction.
// The existence check rides the INSERT itself, the transaction's first
// statement: a concurrent invocation loses at SQLite's single write lock
// and re-evaluates the check on the committed state, so a second admin
// cannot race through between the check and the insert.
pub(crate) async fn create_first_admin_in_pool(
    pool: &SqlitePool,
    name: &str,
) -> Result<CreatedAdmin, CommandError> {
    let name = validate_name(name)?;
    let secret = generate_token()
        .map_err(|error| CommandError::new("persistence", error))?;
    let hash = hash_secret(&secret);
    let created_at = now_millis();
    let mut tx = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO users (name, is_admin, created_at) \
         SELECT ?, 1, ? WHERE NOT EXISTS (SELECT 1 FROM users)",
    )
    .bind(name)
    .bind(created_at)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(CommandError::new(
            "user_exists",
            "A user already exists on this server; the first admin is already bootstrapped.",
        ));
    }
    let user_id = inserted.last_insert_rowid();
    sqlx::query("INSERT INTO user_tokens (user_id, token_hash, created_at) VALUES (?, ?, ?)")
        .bind(user_id)
        .bind(hash)
        .bind(created_at)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(CreatedAdmin {
        user: User {
            id: user_id,
            name: name.into(),
            is_admin: true,
            created_at,
        },
        secret,
    })
}

#[allow(dead_code)] // the admin API lists users in a later phase
pub(crate) async fn list_users_in_pool(pool: &SqlitePool) -> Result<Vec<User>, CommandError> {
    let rows = sqlx::query("SELECT id, name, is_admin, created_at FROM users ORDER BY created_at, id")
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(user_from_row)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

// Deleting a user cascades their tokens (the FK), so every secret stops
// authenticating immediately; like deleted agent tokens, authored history
// stays behind as attribution.
#[allow(dead_code)] // the admin API deletes users in a later phase
pub(crate) async fn delete_user_in_pool(pool: &SqlitePool, id: i64) -> Result<(), CommandError> {
    let result = sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(CommandError::new(
            "invalid_user",
            "The user does not exist.",
        ));
    }
    Ok(())
}

#[allow(dead_code)] // the admin API mints user tokens in a later phase
pub(crate) async fn create_user_token_in_pool(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<CreatedUserToken, CommandError> {
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    if exists.is_none() {
        return Err(CommandError::new(
            "invalid_user",
            "The user does not exist.",
        ));
    }
    let secret = generate_token()
        .map_err(|error| CommandError::new("persistence", error))?;
    let hash = hash_secret(&secret);
    let created_at = now_millis();
    let result =
        sqlx::query("INSERT INTO user_tokens (user_id, token_hash, created_at) VALUES (?, ?, ?)")
            .bind(user_id)
            .bind(hash)
            .bind(created_at)
            .execute(pool)
            .await?;
    Ok(CreatedUserToken {
        token: UserToken {
            id: result.last_insert_rowid(),
            user_id,
            created_at,
            last_used_at: None,
        },
        secret,
    })
}

// Hashes the presented secret and matches the owning user; database
// failures fail closed. The presented secret never reaches storage.
#[allow(dead_code)] // human auth over HTTP arrives in a later phase
pub(crate) async fn verify_user_token_in_pool(pool: &SqlitePool, secret: &str) -> Option<User> {
    let hash = hash_secret(secret);
    let (token_id, id, name, is_admin, created_at): (i64, i64, String, i64, i64) = sqlx::query_as(
        "SELECT user_tokens.id, users.id, users.name, users.is_admin, users.created_at \
         FROM user_tokens JOIN users ON users.id = user_tokens.user_id \
         WHERE user_tokens.token_hash = ?",
    )
    .bind(hash)
    .fetch_optional(pool)
    .await
    .ok()??;
    // Activity recording is best-effort; the verification already decided.
    let _ = sqlx::query("UPDATE user_tokens SET last_used_at = ? WHERE id = ?")
        .bind(now_millis())
        .bind(token_id)
        .execute(pool)
        .await;
    Some(User {
        id,
        name,
        is_admin: is_admin != 0,
        created_at,
    })
}

#[allow(dead_code)] // the admin API revokes user tokens in a later phase
pub(crate) async fn delete_user_token_in_pool(
    pool: &SqlitePool,
    id: i64,
) -> Result<(), CommandError> {
    let result = sqlx::query("DELETE FROM user_tokens WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        return Err(CommandError::new(
            "invalid_user_token",
            "The token does not exist.",
        ));
    }
    Ok(())
}

// Admin-count helpers back the later enforcement phases: destructive
// admin management refuses to remove the last one.
#[allow(dead_code)] // enforcement arrives with human auth in a later phase
pub(crate) async fn count_admins_in_pool(pool: &SqlitePool) -> Result<i64, CommandError> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE is_admin = 1")
        .fetch_one(pool)
        .await?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::test_pool;

    #[tokio::test]
    async fn first_admin_creation_succeeds_on_an_empty_store() {
        let pool = test_pool().await;
        let created = create_first_admin_in_pool(&pool, "  ops  ").await.unwrap();
        assert_eq!(created.user.name, "ops", "the name is trimmed once at the boundary");
        assert!(created.user.is_admin);
        assert!(!created.secret.is_empty());
        assert_ne!(created.secret, hash_secret(&created.secret));
        let users = list_users_in_pool(&pool).await.unwrap();
        assert_eq!(users, [created.user.clone()]);
        let stored_hash: String = sqlx::query_scalar("SELECT token_hash FROM user_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored_hash, hash_secret(&created.secret));
    }

    #[tokio::test]
    async fn a_second_first_admin_refuses() {
        let pool = test_pool().await;
        create_first_admin_in_pool(&pool, "ops").await.unwrap();
        let error = create_first_admin_in_pool(&pool, "second").await.unwrap_err();
        assert_eq!(error.code, "user_exists");
        assert_eq!(list_users_in_pool(&pool).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn verify_user_token_round_trips_and_refuses_wrong_secrets() {
        let pool = test_pool().await;
        let created = create_first_admin_in_pool(&pool, "ops").await.unwrap();
        let user = verify_user_token_in_pool(&pool, &created.secret).await.unwrap();
        assert_eq!(user.id, created.user.id);
        assert_eq!(user.name, "ops");
        assert!(user.is_admin);
        assert!(verify_user_token_in_pool(&pool, "not-a-real-secret").await.is_none());
    }

    #[tokio::test]
    async fn delete_user_cascades_their_tokens() {
        let pool = test_pool().await;
        let created = create_first_admin_in_pool(&pool, "ops").await.unwrap();
        let extra = create_user_token_in_pool(&pool, created.user.id).await.unwrap();
        assert_ne!(extra.secret, created.secret);

        delete_user_in_pool(&pool, created.user.id).await.unwrap();

        assert!(list_users_in_pool(&pool).await.unwrap().is_empty());
        let tokens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tokens, 0);
        assert!(verify_user_token_in_pool(&pool, &created.secret).await.is_none());
        assert!(verify_user_token_in_pool(&pool, &extra.secret).await.is_none());
        assert_eq!(
            delete_user_in_pool(&pool, created.user.id).await.unwrap_err().code,
            "invalid_user"
        );
    }

    #[tokio::test]
    async fn verification_stamps_last_used_at() {
        let pool = test_pool().await;
        let created = create_first_admin_in_pool(&pool, "ops").await.unwrap();
        let before: Option<i64> = sqlx::query_scalar("SELECT last_used_at FROM user_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(before, None);
        verify_user_token_in_pool(&pool, &created.secret).await.unwrap();
        let after: Option<i64> = sqlx::query_scalar("SELECT last_used_at FROM user_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(after.is_some());
    }
}

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
#[derive(Serialize)]
pub struct CreatedAdmin {
    pub user: User,
    pub secret: String,
}

// The carried secret never renders through Debug: the token crosses logs
// and traces wherever the creating face does, and print-once is the whole
// contract. Serialization keeps the secret so the creating client can
// show it once.
impl std::fmt::Debug for CreatedAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedAdmin")
            .field("user", &self.user)
            .field("secret", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Serialize)]
pub struct UserToken {
    pub id: i64,
    pub user_id: i64,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

// The plaintext secret is returned once and never stored.
#[derive(Serialize)]
pub struct CreatedUserToken {
    pub token: UserToken,
    pub secret: String,
}

// Like CreatedAdmin: Debug redacts, Serialize carries the print-once
// secret to the creating admin client.
impl std::fmt::Debug for CreatedUserToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedUserToken")
            .field("token", &self.token)
            .field("secret", &"[redacted]")
            .finish()
    }
}

// A freshly registered member with their first token: the secret exists
// only in this value and is printed once by the admin client.
#[derive(Serialize)]
pub struct CreatedUser {
    pub user: User,
    pub secret: String,
}

impl std::fmt::Debug for CreatedUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedUser")
            .field("user", &self.user)
            .field("secret", &"[redacted]")
            .finish()
    }
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

pub(crate) async fn list_users_in_pool(pool: &SqlitePool) -> Result<Vec<User>, CommandError> {
    let rows = sqlx::query("SELECT id, name, is_admin, created_at FROM users ORDER BY created_at, id")
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(user_from_row)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

// Registers a member with their first token in one write, mirroring the
// first admin's shape minus the bootstrap exclusivity. Names are unique
// (the column enforces it; the pre-check gives the friendly error).
pub(crate) async fn create_user_in_pool(
    pool: &SqlitePool,
    name: &str,
) -> Result<CreatedUser, CommandError> {
    let name = validate_name(name)?;
    let taken: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE name = ?")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    if taken.is_some() {
        return Err(CommandError::new(
            "user_exists",
            format!("A user named {name} already exists."),
        ));
    }
    let secret = generate_token()
        .map_err(|error| CommandError::new("persistence", error))?;
    let hash = hash_secret(&secret);
    let created_at = now_millis();
    let result = sqlx::query("INSERT INTO users (name, is_admin, created_at) VALUES (?, 0, ?)")
        .bind(name)
        .bind(created_at)
        .execute(pool)
        .await?;
    let user_id = result.last_insert_rowid();
    sqlx::query("INSERT INTO user_tokens (user_id, token_hash, created_at) VALUES (?, ?, ?)")
        .bind(user_id)
        .bind(hash)
        .bind(created_at)
        .execute(pool)
        .await?;
    Ok(CreatedUser {
        user: User {
            id: user_id,
            name: name.into(),
            is_admin: false,
            created_at,
        },
        secret,
    })
}

// Deleting a user cascades their tokens (the FK), so every secret stops
// authenticating immediately; like deleted agent tokens, authored history
// stays behind as attribution. The server's one-admin invariant is
// enforced here: the last admin cannot be deleted.
pub(crate) async fn delete_user_in_pool(pool: &SqlitePool, id: i64) -> Result<(), CommandError> {
    let user = user_from_row(
        &sqlx::query("SELECT id, name, is_admin, created_at FROM users WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .map_err(|error| match error {
                sqlx::Error::RowNotFound => {
                    CommandError::new("invalid_user", "The user does not exist.")
                }
                other => CommandError::from(other),
            })?,
    )
    .map_err(CommandError::from)?;
    if user.is_admin && count_admins_in_pool(pool).await? <= 1 {
        return Err(CommandError::new(
            "last_admin",
            "The last admin cannot be deleted; create another admin first.",
        ));
    }
    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// Mints an additional token for an existing user (rotation or a second
// device); the previous tokens stay valid until deleted individually.
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

// Revokes one token without touching the account: a leaked token is
// dropped while the user's other tokens (and the account) survive.
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

// Admin-count helpers back the enforcement: destructive admin management
// refuses to remove the last one.
pub(crate) async fn count_admins_in_pool(pool: &SqlitePool) -> Result<i64, CommandError> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE is_admin = 1")
        .fetch_one(pool)
        .await?;
    Ok(count)
}

// One user's live tokens, oldest first; the caller checks the user exists.
pub(crate) async fn list_user_tokens_in_pool(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Vec<UserToken>, CommandError> {
    let rows = sqlx::query(
        "SELECT id, user_id, created_at, last_used_at FROM user_tokens \
         WHERE user_id = ? ORDER BY created_at, id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            Ok::<UserToken, sqlx::Error>(UserToken {
                id: row.try_get("id")?,
                user_id: row.try_get("user_id")?,
                created_at: row.try_get("created_at")?,
                last_used_at: row.try_get("last_used_at")?,
            })
        })
        .collect::<sqlx::Result<Vec<_>>>()
        .map_err(Into::into)
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

        // The only admin refuses deletion, so a second admin joins first.
        let second = create_user_in_pool(&pool, "root").await.unwrap();
        sqlx::query("UPDATE users SET is_admin = 1 WHERE id = ?")
            .bind(second.user.id)
            .execute(&pool)
            .await
            .unwrap();

        delete_user_in_pool(&pool, created.user.id).await.unwrap();

        let mut survivor = second.user.clone();
        survivor.is_admin = true;
        assert_eq!(list_users_in_pool(&pool).await.unwrap(), [survivor]);
        let tokens: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM user_tokens WHERE user_id = ?",
        )
        .bind(created.user.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(tokens, 0);
        assert!(verify_user_token_in_pool(&pool, &created.secret).await.is_none());
        assert!(verify_user_token_in_pool(&pool, &extra.secret).await.is_none());
        assert!(verify_user_token_in_pool(&pool, &second.secret).await.is_some());
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

    // Debug never carries the secret, on any created-token value; that is
    // the redaction boundary the admin API's print-once contract relies on.
    #[test]
    fn created_values_redact_the_secret_in_debug_but_serialize_it() {
        let admin = CreatedAdmin {
            user: User { id: 1, name: "ops".into(), is_admin: true, created_at: 0 },
            secret: "secret".into(),
        };
        assert!(!format!("{admin:?}").contains("secret\""));
        assert!(serde_json::to_value(&admin).unwrap()["secret"] == "secret");
        let token = CreatedUserToken {
            token: UserToken { id: 1, user_id: 1, created_at: 0, last_used_at: None },
            secret: "secret".into(),
        };
        assert!(!format!("{token:?}").contains("secret\""));
        assert!(serde_json::to_value(&token).unwrap()["secret"] == "secret");
        let user = CreatedUser {
            user: User { id: 2, name: "dana".into(), is_admin: false, created_at: 0 },
            secret: "secret".into(),
        };
        assert!(!format!("{user:?}").contains("secret\""));
        assert!(serde_json::to_value(&user).unwrap()["secret"] == "secret");
    }

    #[tokio::test]
    async fn created_members_get_validated_unique_names_and_a_first_token() {
        let pool = test_pool().await;
        create_first_admin_in_pool(&pool, "ops").await.unwrap();
        let dana = create_user_in_pool(&pool, "  dana ").await.unwrap();
        assert_eq!(dana.user.name, "dana");
        assert!(!dana.user.is_admin);
        assert!(verify_user_token_in_pool(&pool, &dana.secret).await.is_some());

        assert_eq!(
            create_user_in_pool(&pool, "dana").await.unwrap_err().code,
            "user_exists"
        );
        assert_eq!(
            create_user_in_pool(&pool, "   ").await.unwrap_err().code,
            "invalid_user"
        );
        assert_eq!(list_users_in_pool(&pool).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn user_tokens_list_rotate_and_revoke() {
        let pool = test_pool().await;
        let admin = create_first_admin_in_pool(&pool, "ops").await.unwrap();
        let second = create_user_token_in_pool(&pool, admin.user.id).await.unwrap();
        let listed = list_user_tokens_in_pool(&pool, admin.user.id).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_ne!(listed[0].id, listed[1].id);
        assert!(listed.iter().all(|token| token.user_id == admin.user.id));

        // Revoking one token leaves the account and the other token live.
        delete_user_token_in_pool(&pool, second.token.id).await.unwrap();
        assert!(verify_user_token_in_pool(&pool, &second.secret).await.is_none());
        assert!(verify_user_token_in_pool(&pool, &admin.secret).await.is_some());
        assert_eq!(list_user_tokens_in_pool(&pool, admin.user.id).await.unwrap().len(), 1);
        assert_eq!(
            delete_user_token_in_pool(&pool, second.token.id).await.unwrap_err().code,
            "invalid_user_token"
        );
    }

    #[tokio::test]
    async fn deleting_the_last_admin_refuses_but_a_member_deletes() {
        let pool = test_pool().await;
        let admin = create_first_admin_in_pool(&pool, "ops").await.unwrap();
        assert_eq!(
            delete_user_in_pool(&pool, admin.user.id).await.unwrap_err().code,
            "last_admin"
        );

        let dana = create_user_in_pool(&pool, "dana").await.unwrap();
        delete_user_in_pool(&pool, dana.user.id).await.unwrap();
        assert!(verify_user_token_in_pool(&pool, &dana.secret).await.is_none());

        // Two admins: either may go, the survivor keeps the server.
        let second_admin = create_user_in_pool(&pool, "root").await.unwrap();
        sqlx::query("UPDATE users SET is_admin = 1 WHERE id = ?")
            .bind(second_admin.user.id)
            .execute(&pool)
            .await
            .unwrap();
        delete_user_in_pool(&pool, admin.user.id).await.unwrap();
        assert_eq!(count_admins_in_pool(&pool).await.unwrap(), 1);
    }
}

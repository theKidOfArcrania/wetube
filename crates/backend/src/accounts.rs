use anyhow::{Context, Result, bail};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;
use wetube_core::{Role, User, normalize_username, validate_password};

pub fn hash_password(password: &str) -> Result<String> {
    validate_password(password)?;
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| anyhow::anyhow!("password hashing failed: {error}"))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).ok().is_some_and(|hash| {
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    })
}

pub fn random_token() -> String {
    let mut bytes = [0; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn user_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<User> {
    let role = match row.try_get::<&str, _>("role")? {
        "admin" => Role::Admin,
        "viewer" => Role::Viewer,
        _ => bail!("invalid stored user role"),
    };
    Ok(User {
        id: row.try_get("id")?,
        username: row.try_get("username")?,
        role,
    })
}

pub async fn create_user(
    pool: &SqlitePool,
    username: &str,
    password: String,
    role: Role,
) -> Result<()> {
    let username = normalize_username(username)?;
    let hash = tokio::task::spawn_blocking(move || hash_password(&password)).await??;
    sqlx::query("INSERT INTO users (id, username, password_hash, role) VALUES (?, ?, ?, ?)")
        .bind(Uuid::new_v4().to_string())
        .bind(username)
        .bind(hash)
        .bind(match role {
            Role::Admin => "admin",
            Role::Viewer => "viewer",
        })
        .execute(pool)
        .await
        .context("create user; usernames must be unique")?;
    Ok(())
}

pub async fn reset_password(pool: &SqlitePool, username: &str, password: String) -> Result<()> {
    let username = normalize_username(username)?;
    let hash = tokio::task::spawn_blocking(move || hash_password(&password)).await??;
    let mut transaction = pool.begin().await?;
    let result = sqlx::query("UPDATE users SET password_hash = ? WHERE username = ?")
        .bind(hash)
        .bind(&username)
        .execute(&mut *transaction)
        .await?;
    if result.rows_affected() != 1 {
        bail!("user not found");
    }
    sqlx::query("DELETE FROM sessions WHERE user_id = (SELECT id FROM users WHERE username = ?)")
        .bind(username)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[tokio::test]
    async fn password_reset_revokes_existing_sessions() {
        let dir = tempfile::tempdir().unwrap();
        db::initialize(dir.path()).await.unwrap();
        let pool = db::connect(dir.path()).await.unwrap();
        create_user(
            &pool,
            "Alice",
            "old password long enough".into(),
            Role::Admin,
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions SELECT 'token', id, 'csrf', 9999999999 FROM users WHERE username = 'alice'",
        )
        .execute(&pool)
        .await
        .unwrap();
        reset_password(&pool, "Alice", "new password long enough".into())
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let hash: String =
            sqlx::query_scalar("SELECT password_hash FROM users WHERE username = 'alice'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!verify_password("old password long enough", &hash));
        assert!(verify_password("new password long enough", &hash));
    }
}

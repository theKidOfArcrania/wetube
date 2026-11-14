use anyhow::Result;
use sqlx::{Row, SqlitePool};
use wetube_core::{PlaybackLease, TrashMovie};

pub const RETENTION_SECONDS: i64 = 30 * 24 * 60 * 60;
pub const LEASE_SECONDS: i64 = 120;

pub async fn list(pool: &SqlitePool) -> Result<Vec<TrashMovie>> {
    Ok(sqlx::query("SELECT m.id,m.title,t.* FROM movie_trash t JOIN movies m ON m.id=t.movie_id ORDER BY t.deleted_at DESC,m.id")
        .fetch_all(pool).await?.into_iter().map(|row| TrashMovie {
            id: row.get("id"), title: row.get("title"), deleted_at: row.get("deleted_at"),
            purge_after: row.get("purge_after"),
            purging: row.get::<Option<i64>,_>("purge_started_at").is_some(), error: row.get("error"),
        }).collect())
}

pub async fn delete(pool: &SqlitePool, id: &str, at: i64) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let changed = sqlx::query("UPDATE movies SET status='trashed' WHERE id=? AND status='ready'")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    if changed {
        sqlx::query(
            "INSERT INTO movie_trash(movie_id,deleted_at,purge_after,provenance_complete)
            VALUES(?,?,?,EXISTS(SELECT 1 FROM movie_outputs WHERE movie_id=?))",
        )
        .bind(id)
        .bind(at)
        .bind(at + RETENTION_SECONDS)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

pub async fn restore(pool: &SqlitePool, id: &str, at: i64) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let changed = sqlx::query(
        "DELETE FROM movie_trash WHERE movie_id=? AND purge_after>? AND purge_started_at IS NULL",
    )
    .bind(id)
    .bind(at)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if changed {
        sqlx::query("UPDATE movies SET status='ready' WHERE id=? AND status='trashed'")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

/// Authorization and purge claim both acquire SQLite's writer lock first.
/// Ready movies can reauthorize their bound login after inactivity/pruning.
/// Trash never revives expired authorization, including after a worker restart.
pub async fn authorize(
    pool: &SqlitePool,
    movie: &str,
    session: &str,
    lease: Option<&str>,
    at: i64,
) -> Result<Option<PlaybackLease>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let allowed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM movies m WHERE m.id=?
         AND NOT EXISTS(SELECT 1 FROM movie_trash t WHERE t.movie_id=m.id AND t.purge_started_at IS NOT NULL)
         AND ((? IS NULL AND m.status='ready') OR EXISTS(
          SELECT 1 FROM playback_lease_bindings b WHERE b.id=? AND b.movie_id=m.id AND b.session_hash=?
          AND (m.status='ready' OR EXISTS(SELECT 1 FROM playback_leases l WHERE l.id=b.id AND l.expires_at>?))))
         AND EXISTS(SELECT 1 FROM sessions WHERE token_hash=? AND expires_at>?))"
    ).bind(movie).bind(lease).bind(lease).bind(session).bind(at).bind(session).bind(at)
        .fetch_one(&mut *tx).await?;
    if !allowed {
        return Ok(None);
    }
    let id = lease
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let expires_at = at + LEASE_SECONDS;
    if lease.is_none() {
        sqlx::query("INSERT INTO playback_lease_bindings(id,movie_id,session_hash) VALUES(?,?,?)")
            .bind(&id)
            .bind(movie)
            .bind(session)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "INSERT INTO playback_leases(id,movie_id,session_hash,expires_at) VALUES(?,?,?,?)
        ON CONFLICT(id) DO UPDATE SET expires_at=excluded.expires_at",
    )
    .bind(&id)
    .bind(movie)
    .bind(session)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(PlaybackLease { id, expires_at }))
}

pub async fn valid(
    pool: &SqlitePool,
    movie: &str,
    session: &str,
    lease: &str,
    at: i64,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM playback_leases l JOIN movies m ON m.id=l.movie_id
        WHERE l.id=? AND l.movie_id=? AND l.session_hash=? AND l.expires_at>?
        AND NOT EXISTS(SELECT 1 FROM movie_trash t WHERE t.movie_id=m.id AND t.purge_started_at IS NOT NULL))")
        .bind(lease).bind(movie).bind(session).bind(at).fetch_one(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ready_playback_reauthorizes_after_inactivity_and_pruning_without_crossing_scope_or_trash()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        crate::db::initialize(directory.path()).await?;
        let pool = crate::db::connect(directory.path()).await?;
        sqlx::query("INSERT INTO movies VALUES('movie','Local',NULL,'','ready'),('other','Other',NULL,'','ready')").execute(&pool).await?;
        sqlx::query("INSERT INTO users VALUES('user','user','hash','viewer')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO sessions VALUES('session','user','csrf',99999999),('other','user','csrf',99999999)").execute(&pool).await?;
        let lease = authorize(&pool, "movie", "session", None, 100)
            .await?
            .unwrap();
        assert!(
            authorize(&pool, "movie", "session", Some(&lease.id), 221)
                .await?
                .is_some()
        );
        sqlx::query("DELETE FROM playback_leases")
            .execute(&pool)
            .await?;
        assert!(
            authorize(&pool, "movie", "other", Some(&lease.id), 342)
                .await?
                .is_none()
        );
        assert!(
            authorize(&pool, "other", "session", Some(&lease.id), 342)
                .await?
                .is_none()
        );
        assert!(
            authorize(&pool, "movie", "session", Some("unknown"), 342)
                .await?
                .is_none()
        );
        assert_eq!(
            authorize(&pool, "movie", "session", Some(&lease.id), 342)
                .await?
                .unwrap()
                .expires_at,
            462
        );
        assert!(delete(&pool, "movie", 343).await?);
        assert!(
            authorize(&pool, "movie", "session", Some(&lease.id), 462)
                .await?
                .is_none()
        );
        sqlx::query("DELETE FROM playback_leases")
            .execute(&pool)
            .await?;
        assert!(
            authorize(&pool, "movie", "session", Some(&lease.id), 463)
                .await?
                .is_none()
        );
        assert!(restore(&pool, "movie", 464).await?);
        assert!(
            authorize(&pool, "movie", "session", Some(&lease.id), 465)
                .await?
                .is_some()
        );
        assert!(delete(&pool, "movie", 466).await?);
        sqlx::query("UPDATE movie_trash SET purge_started_at=467")
            .execute(&pool)
            .await?;
        assert!(
            authorize(&pool, "movie", "session", Some(&lease.id), 467)
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn trash_preserves_associations_restore_clears_deadline_and_leases_never_resurrect()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        crate::db::initialize(directory.path()).await?;
        let pool = crate::db::connect(directory.path()).await?;
        sqlx::query("INSERT INTO movies VALUES('movie','Local',2000,'Original','ready')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO users VALUES('user','user','hash','viewer')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO sessions VALUES('session','user','csrf',99999999),('other','user','csrf',99999999)").execute(&pool).await?;
        sqlx::query("INSERT INTO playback_progress VALUES('user','movie',123,1)")
            .execute(&pool)
            .await?;
        let labels = wetube_core::MovieLabels {
            categories: vec!["Family".into()],
            tags: vec![],
        };
        crate::labels::update(
            &pool,
            "movie",
            &wetube_core::UpdateMovieLabels {
                expected: Default::default(),
                labels: labels.clone(),
            },
        )
        .await?;
        sqlx::query(
            "INSERT INTO tmdb_metadata VALUES('movie',7,'Provider',2001,'Synopsis',1,99999999)",
        )
        .execute(&pool)
        .await?;
        let lease = authorize(&pool, "movie", "session", None, 100)
            .await?
            .unwrap();
        assert!(delete(&pool, "movie", 101).await?);
        assert!(!delete(&pool, "movie", 102).await?);
        assert!(
            authorize(&pool, "movie", "session", None, 102)
                .await?
                .is_none()
        );
        assert!(
            authorize(&pool, "movie", "other", Some(&lease.id), 102)
                .await?
                .is_none()
        );
        assert!(
            authorize(&pool, "movie", "session", Some(&lease.id), 219)
                .await?
                .is_some()
        );
        assert!(
            authorize(&pool, "movie", "session", Some(&lease.id), 339)
                .await?
                .is_none()
        );
        assert!(restore(&pool, "movie", 101 + RETENTION_SECONDS - 1).await?);
        assert!(list(&pool).await?.is_empty());
        assert_eq!(crate::labels::for_movie(&pool, "movie").await?, labels);
        assert_eq!(
            sqlx::query_scalar::<_, f64>("SELECT position_seconds FROM playback_progress")
                .fetch_one(&pool)
                .await?,
            123.0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT tmdb_id FROM tmdb_metadata")
                .fetch_one(&pool)
                .await?,
            7
        );
        assert!(delete(&pool, "movie", 400).await?);
        assert!(!restore(&pool, "movie", 400 + RETENTION_SECONDS).await?);
        assert!(!restore(&pool, "movie", 401 + RETENTION_SECONDS).await?);
        assert_eq!(list(&pool).await?[0].purge_after, 400 + RETENTION_SECONDS);
        pool.close().await;
        Ok(())
    }
}

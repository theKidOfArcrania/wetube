use anyhow::{Result, ensure};
use sqlx::{SqliteConnection, SqlitePool};
use wetube_core::{MetadataMatch, TmdbSource};

// Shorter than six calendar months, including February.
pub const CACHE_SECONDS: i64 = 150 * 24 * 60 * 60;

pub fn expires_at(source: &TmdbSource) -> Result<i64> {
    ensure!(
        source.id > 0 && source.fetched_at > 0,
        "invalid TMDB source"
    );
    source
        .fetched_at
        .checked_add(CACHE_SECONDS)
        .ok_or_else(|| anyhow::anyhow!("invalid TMDB timestamp"))
}

pub fn validate_selection(source: &TmdbSource, now: i64) -> Result<()> {
    ensure!(
        source.fetched_at <= now && source.fetched_at >= now - 86400,
        "TMDB selection expired; search again before publication"
    );
    expires_at(source)?;
    Ok(())
}

pub async fn store(
    connection: &mut SqliteConnection,
    movie: &str,
    metadata: &MetadataMatch,
) -> Result<()> {
    let expires = expires_at(&TmdbSource {
        id: metadata.id,
        fetched_at: metadata.fetched_at,
    })?;
    ensure!(expires > super::now(), "TMDB metadata expired");
    sqlx::query(
        "INSERT INTO tmdb_metadata(movie_id,tmdb_id,title,year,description,fetched_at,expires_at)
         VALUES (?,?,?,?,?,?,?) ON CONFLICT(movie_id) DO UPDATE SET
         tmdb_id=excluded.tmdb_id,title=excluded.title,year=excluded.year,
         description=excluded.description,fetched_at=excluded.fetched_at,expires_at=excluded.expires_at",
    )
    .bind(movie)
    .bind(metadata.id)
    .bind(&metadata.title)
    .bind(metadata.year)
    .bind(&metadata.description)
    .bind(metadata.fetched_at)
    .bind(expires)
    .execute(connection)
    .await?;
    Ok(())
}

pub async fn expire(pool: &SqlitePool, now: i64) -> Result<()> {
    sqlx::query(
        "UPDATE tmdb_metadata SET title=NULL,year=NULL,description=NULL
         WHERE expires_at<=? AND title IS NOT NULL",
    )
    .bind(now)
    .execute(pool)
    .await?;
    // Test the current JSON in the same statement: a concurrent refresh/re-review
    // must not be erased using a stale selection read by the sweeper.
    sqlx::query(
        "UPDATE conversion_jobs SET publication=NULL,
         phase=CASE WHEN state='queued_publication' THEN 'awaiting_review' ELSE phase END,
         error=CASE WHEN state IN ('queued_publication','publishing') THEN 'Movie metadata expired; review metadata again.' ELSE error END,
         state=CASE state WHEN 'queued_publication' THEN 'needs_review' WHEN 'publishing' THEN 'failed' ELSE state END
         WHERE json_extract(publication,'$.tmdb.fetched_at') <= ?",
    ).bind(now - CACHE_SECONDS).execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;
    use wetube_core::PublishRequest;

    #[tokio::test]
    async fn expiry_scrubs_duplicates_preserves_manual_media_and_allows_explicit_refresh()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        crate::db::initialize(directory.path()).await?;
        let pool = crate::db::connect(directory.path()).await?;
        let now = crate::now();
        sqlx::query("INSERT INTO movies VALUES ('movie','local.iso',NULL,'','ready')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO users VALUES ('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        let old = PublishRequest {
            title: "Provider title".into(),
            year: Some(2001),
            description: "Provider text".into(),
            movie_sha256: "a".repeat(64),
            confirm: true,
            tmdb: Some(TmdbSource {
                id: 12,
                fetched_at: now - CACHE_SECONDS,
            }),
        };
        let mut manual = old.clone();
        manual.tmdb = None;
        let mut fresh = old.clone();
        fresh.tmdb.as_mut().unwrap().fetched_at = now;
        for (id, state, publication) in [
            ("queued", "queued_publication", &old),
            ("running", "publishing", &old),
            ("failed", "failed", &old),
            ("manual", "queued_publication", &manual),
            ("fresh", "queued_publication", &fresh),
        ] {
            sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at) VALUES(?,'admin','local.iso','bound',1,1,'needs_review',1)").bind(id).execute(&pool).await?;
            sqlx::query("INSERT INTO conversion_jobs(id,probe_job_id,requested_by,state,created_at,video_treatment,publication) VALUES(?,?,'admin',?,1,'preserve',?)")
                .bind(id).bind(id).bind(state).bind(serde_json::to_string(publication)?).execute(&pool).await?;
        }
        let metadata = MetadataMatch {
            id: 12,
            title: "Provider title".into(),
            year: Some(2001),
            description: "Provider text".into(),
            fetched_at: now,
        };
        store(&mut *pool.acquire().await?, "movie", &metadata).await?;
        sqlx::query("UPDATE tmdb_metadata SET expires_at=?")
            .bind(now)
            .execute(&pool)
            .await?;
        expire(&pool, now).await?;
        let row = sqlx::query("SELECT * FROM tmdb_metadata")
            .fetch_one(&pool)
            .await?;
        assert_eq!(row.get::<i64, _>("tmdb_id"), 12);
        assert!(row.get::<Option<String>, _>("title").is_none());
        assert!(row.get::<Option<i32>, _>("year").is_none());
        assert!(row.get::<Option<String>, _>("description").is_none());
        for (id, expected) in [
            ("queued", "needs_review"),
            ("running", "failed"),
            ("failed", "failed"),
        ] {
            let row = sqlx::query("SELECT state,publication FROM conversion_jobs WHERE id=?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
            assert_eq!(row.get::<String, _>("state"), expected);
            assert!(row.get::<Option<String>, _>("publication").is_none());
        }
        for id in ["fresh", "manual"] {
            let row = sqlx::query("SELECT state,publication FROM conversion_jobs WHERE id=?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
            assert_eq!(row.get::<String, _>("state"), "queued_publication");
            assert!(row.get::<Option<String>, _>("publication").is_some());
        }
        store(&mut *pool.acquire().await?, "movie", &metadata).await?;
        expire(&pool, now).await?;
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT title FROM tmdb_metadata")
                .fetch_one(&pool)
                .await?,
            "Provider title"
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT title FROM movies")
                .fetch_one(&pool)
                .await?,
            "local.iso"
        );
        let expired = MetadataMatch {
            fetched_at: now - CACHE_SECONDS,
            ..metadata
        };
        assert!(
            store(&mut *pool.acquire().await?, "movie", &expired)
                .await
                .is_err()
        );
        assert!(
            validate_selection(
                &TmdbSource {
                    id: 12,
                    fetched_at: now - 86401
                },
                now
            )
            .is_err()
        );
        assert!(
            validate_selection(
                &TmdbSource {
                    id: 12,
                    fetched_at: now + 1
                },
                now
            )
            .is_err()
        );
        pool.close().await;
        Ok(())
    }
}

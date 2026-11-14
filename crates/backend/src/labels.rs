use anyhow::{Result, ensure};
use sqlx::{Row, SqliteConnection, SqlitePool};
use std::collections::{BTreeMap, BTreeSet};
use wetube_core::{MovieLabels, UpdateMovieLabels, label_key, normalize_label_name};

pub fn normalize(names: &[String]) -> Result<Vec<String>> {
    ensure!(
        names.len() <= 128,
        "Submit at most 128 label entries of each kind"
    );
    let mut unique = BTreeMap::new();
    for name in names {
        ensure!(
            !name.chars().any(char::is_control),
            "Labels cannot contain control characters"
        );
        let name = normalize_label_name(name);
        ensure!(
            !name.is_empty() && name.len() <= 80,
            "Labels must be 1–80 bytes"
        );
        unique.entry(label_key(&name)).or_insert(name);
    }
    ensure!(
        unique.len() <= 32,
        "Use at most 32 unique labels of each kind"
    );
    Ok(unique.into_values().collect())
}

pub fn validate(labels: &MovieLabels) -> Result<MovieLabels> {
    Ok(MovieLabels {
        categories: normalize(&labels.categories)?,
        tags: normalize(&labels.tags)?,
    })
}

fn from_rows(rows: Vec<sqlx::sqlite::SqliteRow>) -> MovieLabels {
    let mut labels = MovieLabels::default();
    for row in rows {
        let target = if row.get::<String, _>("kind") == "category" {
            &mut labels.categories
        } else {
            &mut labels.tags
        };
        target.push(row.get("name"));
    }
    labels
}

async fn read(connection: &mut SqliteConnection, id: &str) -> Result<MovieLabels> {
    Ok(from_rows(
        sqlx::query(
            "SELECT l.kind,l.name FROM labels l JOIN movie_labels ml ON ml.label_id=l.id
         WHERE ml.movie_id=? ORDER BY l.canonical",
        )
        .bind(id)
        .fetch_all(connection)
        .await?,
    ))
}

pub async fn for_movie(pool: &SqlitePool, id: &str) -> Result<MovieLabels> {
    read(&mut *pool.acquire().await?, id).await
}

pub async fn assignments(pool: &SqlitePool) -> Result<BTreeMap<String, MovieLabels>> {
    let rows = sqlx::query(
        "SELECT ml.movie_id,l.kind,l.name FROM movie_labels ml JOIN labels l ON l.id=ml.label_id
         ORDER BY l.canonical",
    )
    .fetch_all(pool)
    .await?;
    let mut result = BTreeMap::<String, MovieLabels>::new();
    for row in rows {
        let labels = result.entry(row.get("movie_id")).or_default();
        if row.get::<String, _>("kind") == "category" {
            labels.categories.push(row.get("name"));
        } else {
            labels.tags.push(row.get("name"));
        }
    }
    Ok(result)
}

pub async fn catalog(pool: &SqlitePool) -> Result<MovieLabels> {
    Ok(from_rows(
        sqlx::query("SELECT kind,name FROM labels ORDER BY canonical")
            .fetch_all(pool)
            .await?,
    ))
}

#[derive(Debug, PartialEq)]
pub enum UpdateResult {
    Saved(MovieLabels),
    Missing,
    Conflict,
}

pub async fn update(
    pool: &SqlitePool,
    id: &str,
    request: &UpdateMovieLabels,
) -> Result<UpdateResult> {
    let labels = validate(&request.labels)?;
    let expected = validate(&request.expected)?;
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM movies WHERE id=? AND status='ready')")
            .bind(id)
            .fetch_one(&mut *transaction)
            .await?;
    if !exists {
        return Ok(UpdateResult::Missing);
    }
    let current = read(&mut transaction, id).await?;
    fn keys(labels: &[String]) -> BTreeSet<String> {
        labels.iter().map(|name| label_key(name)).collect()
    }
    if keys(&current.categories) != keys(&expected.categories)
        || keys(&current.tags) != keys(&expected.tags)
    {
        return Ok(UpdateResult::Conflict);
    }
    sqlx::query("DELETE FROM movie_labels WHERE movie_id=?")
        .bind(id)
        .execute(&mut *transaction)
        .await?;
    for (kind, names) in [("category", labels.categories), ("tag", labels.tags)] {
        for name in names {
            let canonical = label_key(&name);
            sqlx::query("INSERT INTO labels(kind,name,canonical) VALUES(?,?,?) ON CONFLICT(kind,canonical) DO NOTHING")
                .bind(kind).bind(name).bind(&canonical).execute(&mut *transaction).await?;
            sqlx::query("INSERT INTO movie_labels(movie_id,label_id) SELECT ?,id FROM labels WHERE kind=? AND canonical=?")
                .bind(id).bind(kind).bind(canonical).execute(&mut *transaction).await?;
        }
    }
    let saved = read(&mut transaction, id).await?;
    transaction.commit().await?;
    Ok(UpdateResult::Saved(saved))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn labels_are_atomic_reusable_persistent_and_conflict_checked() -> Result<()> {
        let directory = tempfile::tempdir()?;
        crate::db::initialize(directory.path()).await?;
        let pool = crate::db::connect(directory.path()).await?;
        sqlx::query("INSERT INTO movies VALUES('one','Original',1999,'Description','ready'),('two','Other',NULL,'','ready')")
            .execute(&pool).await?;
        let mut request = UpdateMovieLabels {
            expected: MovieLabels::default(),
            labels: MovieLabels {
                categories: vec![
                    " Family   Night ".into(),
                    "family night".into(),
                    "Adventure".into(),
                ],
                tags: vec!["FAVORITE".into(), "favorite".into()],
            },
        };
        let UpdateResult::Saved(first) = update(&pool, "one", &request).await? else {
            panic!()
        };
        assert_eq!(first.categories, ["Adventure", "Family Night"]);
        assert_eq!(first.tags, ["FAVORITE"]);
        assert_eq!(
            update(&pool, "one", &request).await?,
            UpdateResult::Conflict
        );
        assert_eq!(
            update(&pool, "absent", &request).await?,
            UpdateResult::Missing
        );
        request.labels.categories = vec!["family night".into()];
        let UpdateResult::Saved(other) = update(&pool, "two", &request).await? else {
            panic!()
        };
        assert_eq!(other.categories, ["Family Night"]);
        request.expected = first.clone();
        for bad in [
            vec!["valid".into(), "".into()],
            vec!["x".repeat(81)],
            (0..33).map(|index| format!("label {index}")).collect(),
            vec!["bad\nlabel".into()],
        ] {
            request.labels.tags = bad;
            assert!(update(&pool, "one", &request).await.is_err());
            assert_eq!(for_movie(&pool, "one").await?, first);
        }
        pool.close().await;
        let pool = crate::db::connect(directory.path()).await?;
        assert_eq!(for_movie(&pool, "one").await?, first);
        request.labels = MovieLabels::default();
        assert_eq!(
            update(&pool, "one", &request).await?,
            UpdateResult::Saved(MovieLabels::default())
        );
        assert_eq!(for_movie(&pool, "two").await?, other);
        assert!(
            catalog(&pool)
                .await?
                .categories
                .contains(&"Adventure".into())
        );
        let title: String = sqlx::query_scalar("SELECT title FROM movies WHERE id='one'")
            .fetch_one(&pool)
            .await?;
        assert_eq!(title, "Original");
        sqlx::query("DELETE FROM movies WHERE id='two'")
            .execute(&pool)
            .await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM movie_labels")
                .fetch_one(&pool)
                .await?,
            0
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn labels_limit_counts_unique_names_after_suggestion_deduplication() -> Result<()> {
        let directory = tempfile::tempdir()?;
        crate::db::initialize(directory.path()).await?;
        let pool = crate::db::connect(directory.path()).await?;
        sqlx::query("INSERT INTO movies VALUES('movie','Original',NULL,'','ready')")
            .execute(&pool)
            .await?;
        let mut categories = (0..28)
            .map(|index| format!("Category {index}"))
            .collect::<Vec<_>>();
        categories.extend(["Drama".into(), "Family".into()]);
        let initial = MovieLabels {
            categories,
            tags: Vec::new(),
        };
        let UpdateResult::Saved(initial) = update(
            &pool,
            "movie",
            &UpdateMovieLabels {
                expected: MovieLabels::default(),
                labels: initial,
            },
        )
        .await?
        else {
            panic!()
        };
        let mut request = UpdateMovieLabels {
            expected: initial.clone(),
            labels: initial,
        };
        request
            .labels
            .categories
            .extend([" drama ".into(), "FAMILY".into(), "Thriller".into()]);
        let UpdateResult::Saved(saved) = update(&pool, "movie", &request).await? else {
            panic!()
        };
        assert_eq!(saved.categories.len(), 31);
        assert!(saved.categories.contains(&"Drama".into()));
        assert!(saved.categories.contains(&"Family".into()));
        assert!(saved.categories.contains(&"Thriller".into()));
        request.expected = saved.clone();
        request
            .labels
            .categories
            .extend(["Unique 32".into(), "Unique 33".into()]);
        assert!(update(&pool, "movie", &request).await.is_err());
        assert_eq!(for_movie(&pool, "movie").await?, saved);
        assert!(normalize(&vec!["duplicate".into(); 129]).is_err());
        pool.close().await;
        Ok(())
    }
}

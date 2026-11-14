use anyhow::Result;
use sqlx::{Row, SqlitePool};
use wetube_core::{ImportEntry, ImportJob, ImportPage};

const PERSONAL: &str = "WITH personal AS (
    SELECT j.*, j.can_archive AND COALESCE(p.archived,j.default_history) AS in_history
    FROM import_lifecycle j LEFT JOIN import_preferences p
    ON p.user_id=? AND p.kind=j.kind AND p.job_id=j.id
)";

pub async fn page(
    pool: &SqlitePool,
    user: &str,
    history: bool,
    before: Option<&str>,
    limit: u32,
) -> Result<ImportPage> {
    anyhow::ensure!((1..=50).contains(&limit), "invalid page limit");
    let mut tx = pool.begin().await?;
    let cursor = if let Some(before) = before {
        anyhow::ensure!(valid_cursor(before), "invalid cursor");
        let (kind, id) = before.split_once(':').unwrap();
        sqlx::query("SELECT created_at,kind,id FROM import_lifecycle WHERE kind=? AND id=?")
            .bind(kind)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
    } else {
        None
    };
    let counts = sqlx::query(&format!(
        "{PERSONAL} SELECT COUNT(*) AS total,COALESCE(SUM(in_history),0) AS history_count FROM personal"
    ))
    .bind(user)
    .fetch_one(&mut *tx)
    .await?;
    let history_count = counts.get::<i64, _>("history_count") as u64;
    let total = if history {
        history_count
    } else {
        counts.get::<i64, _>("total") as u64 - history_count
    };
    if before.is_some() && cursor.is_none() {
        return Ok(ImportPage {
            entries: Vec::new(),
            total,
            history_count,
            next: None,
        });
    }
    let rows = sqlx::query(&format!(
        "{PERSONAL} SELECT kind,id,can_archive FROM personal WHERE in_history=?
         AND (? IS NULL OR created_at < ? OR (created_at=? AND (kind,id)>(?,?)))
         ORDER BY created_at DESC,kind,id LIMIT ?"
    ))
    .bind(user)
    .bind(history)
    .bind(cursor.as_ref().map(|row| row.get::<i64, _>("created_at")))
    .bind(cursor.as_ref().map(|row| row.get::<i64, _>("created_at")))
    .bind(cursor.as_ref().map(|row| row.get::<i64, _>("created_at")))
    .bind(cursor.as_ref().map(|row| row.get::<String, _>("kind")))
    .bind(cursor.as_ref().map(|row| row.get::<String, _>("id")))
    .bind(limit + 1)
    .fetch_all(&mut *tx)
    .await?;
    let next = if rows.len() > limit as usize {
        let row = &rows[limit as usize - 1];
        Some(format!(
            "{}:{}",
            row.get::<String, _>("kind"),
            row.get::<String, _>("id")
        ))
    } else {
        None
    };
    let mut entries = Vec::with_capacity(rows.len().min(limit as usize));
    for row in rows.into_iter().take(limit as usize) {
        let id: String = row.get("id");
        let item = match row.get::<&str, _>("kind") {
            "scan" => ImportJob::Scan(super::scans::from_row(
                &sqlx::query("SELECT * FROM scan_jobs WHERE id=?")
                    .bind(&id)
                    .fetch_one(&mut *tx)
                    .await?,
            )?),
            "probe" => ImportJob::Probe(super::probe_jobs::from_row(
                &sqlx::query("SELECT * FROM probe_jobs WHERE id=?")
                    .bind(&id)
                    .fetch_one(&mut *tx)
                    .await?,
            )?),
            "conversion" => ImportJob::Conversion(super::conversions::from_row(
                &sqlx::query("SELECT * FROM conversion_jobs WHERE id=?")
                    .bind(&id)
                    .fetch_one(&mut *tx)
                    .await?,
            )?),
            _ => unreachable!("closed lifecycle kinds"),
        };
        entries.push(ImportEntry {
            item,
            can_archive: row.get("can_archive"),
        });
    }
    tx.commit().await?;
    Ok(ImportPage {
        entries,
        total,
        history_count,
        next,
    })
}

pub fn valid_cursor(cursor: &str) -> bool {
    cursor.len() <= 64
        && cursor.split_once(':').is_some_and(|(kind, id)| {
            matches!(kind, "scan" | "probe" | "conversion") && uuid::Uuid::parse_str(id).is_ok()
        })
}

/// One statement validates current durable state and writes only this user's preference.
pub async fn set_archived(
    pool: &SqlitePool,
    user: &str,
    kind: &str,
    id: &str,
    archived: bool,
) -> Result<bool> {
    let result = sqlx::query(
        "INSERT INTO import_preferences(user_id,kind,job_id,archived)
         SELECT ?,kind,id,? FROM import_lifecycle WHERE kind=? AND id=? AND can_archive
         ON CONFLICT(user_id,kind,job_id) DO UPDATE SET archived=excluded.archived",
    )
    .bind(user)
    .bind(archived)
    .bind(kind)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

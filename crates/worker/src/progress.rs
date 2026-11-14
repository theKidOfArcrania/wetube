use sqlx::SqlitePool;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use wetube_core::{MediaProgress, MediaWorkflow};

struct Current {
    snapshot: MediaProgress,
    started: Instant,
    last_advance: Instant,
    high_water: f64,
}

#[derive(Clone)]
pub(crate) struct Progress(Arc<Mutex<Current>>);

pub(crate) struct Recording {
    pub progress: Progress,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Progress {
    pub fn stage(&self, stage: usize, phase: &str, total: Option<f64>, unit: &str) {
        let mut current = self.0.lock().unwrap();
        current.snapshot.stage = stage;
        current.snapshot.phase = phase.to_owned();
        current.snapshot.completed_units = total.map(|_| 0.0);
        current.snapshot.total_units = total.filter(|n| n.is_finite() && *n > 0.0);
        current.snapshot.unit = unit.to_owned();
        current.started = Instant::now();
        current.last_advance = current.started;
        current.high_water = 0.0;
    }

    pub fn units(&self, done: f64) {
        let mut current = self.0.lock().unwrap();
        if done.is_finite() && done >= 0.0 {
            current.snapshot.completed_units = Some(done);
            if done > current.high_water {
                current.last_advance = Instant::now();
                current.high_water = done;
            }
        }
    }

    pub fn ffmpeg_line(&self, line: &str) {
        if let Some(value) = line.strip_prefix("out_time_us=")
            && let Ok(value) = value.trim().parse::<i64>()
            && value >= 0
        {
            self.units(value as f64 / 1_000_000.0);
        }
    }

    fn snapshot(&self) -> MediaProgress {
        let current = self.0.lock().unwrap();
        let mut snapshot = current.snapshot.clone();
        snapshot.elapsed_seconds = current.started.elapsed().as_secs_f64();
        snapshot.updated_at = wetube_backend::now();
        // A stalled tool has no defensible current throughput or ETA.
        if current.last_advance.elapsed() > Duration::from_secs(10) {
            snapshot.total_units = None;
        }
        snapshot
    }
}

impl Recording {
    pub fn start(
        pool: &SqlitePool,
        table: &'static str,
        id: &str,
        workflow: MediaWorkflow,
        stage: usize,
    ) -> Self {
        let query = match table {
            "scan_jobs" => "UPDATE scan_jobs SET progress=? WHERE id=?",
            "probe_jobs" => "UPDATE probe_jobs SET progress=? WHERE id=?",
            "conversion_jobs" => "UPDATE conversion_jobs SET progress=? WHERE id=?",
            _ => unreachable!("fixed progress table"),
        };
        let progress = Progress(Arc::new(Mutex::new(Current {
            snapshot: MediaProgress {
                workflow,
                stage,
                phase: workflow.stages()[stage].into(),
                elapsed_seconds: 0.0,
                completed_units: None,
                total_units: None,
                unit: String::new(),
                updated_at: wetube_backend::now(),
            },
            started: Instant::now(),
            last_advance: Instant::now(),
            high_water: 0.0,
        })));
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let writer = progress.clone();
        let pool = pool.clone();
        let id = id.to_owned();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_warning = None::<Instant>;
            loop {
                let finished = tokio::select! {
                    _ = interval.tick() => false,
                    _ = &mut stopped => true,
                };
                let result: anyhow::Result<()> = async {
                    sqlx::query(query)
                        .bind(serde_json::to_string(&writer.snapshot())?)
                        .bind(&id)
                        .execute(&pool)
                        .await?;
                    Ok(())
                }
                .await;
                if let Err(error) = result
                    && last_warning.is_none_or(|last| last.elapsed() >= Duration::from_secs(30))
                {
                    let error: String = error.to_string().chars().take(512).collect();
                    tracing::warn!(%error, job_id=%id, "unable to persist optional media progress; later snapshots will retry");
                    last_warning = Some(Instant::now());
                }
                if finished {
                    break;
                }
            }
        });
        Self {
            progress,
            stop,
            task,
        }
    }

    pub async fn finish(self) {
        let _ = self.stop.send(());
        let mut task = self.task;
        match tokio::time::timeout(Duration::from_secs(1), &mut task).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => tracing::warn!("optional media progress task did not finish normally"),
            Err(_) => {
                task.abort();
                let _ = task.await;
                tracing::warn!(
                    "optional final media progress flush exceeded one second; snapshot omitted"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    #[tokio::test]
    async fn telemetry_write_errors_retry_without_overriding_authoritative_results() -> Result<()> {
        let pool = SqlitePool::connect("sqlite::memory:").await?;
        sqlx::query("CREATE TABLE scan_jobs(id TEXT,state TEXT,progress TEXT)")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO scan_jobs(id,state) VALUES ('scan','scanning')")
            .execute(&pool)
            .await?;
        sqlx::query("CREATE TRIGGER reject_progress BEFORE UPDATE OF progress ON scan_jobs BEGIN SELECT RAISE(FAIL,'injected telemetry write failure'); END").execute(&pool).await?;
        let recording = Recording::start(&pool, "scan_jobs", "scan", MediaWorkflow::Scan, 2);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !recording.task.is_finished(),
            "one failed write must not kill the snapshot recorder"
        );
        sqlx::query("DROP TRIGGER reject_progress")
            .execute(&pool)
            .await?;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let progress: Option<String> = sqlx::query_scalar("SELECT progress FROM scan_jobs")
                    .fetch_one(&pool)
                    .await?;
                if progress.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            anyhow::Ok(())
        })
        .await??;
        sqlx::query("CREATE TRIGGER reject_progress BEFORE UPDATE OF progress ON scan_jobs BEGIN SELECT RAISE(FAIL,'injected final telemetry failure'); END").execute(&pool).await?;
        sqlx::query("UPDATE scan_jobs SET state='needs_review' WHERE id='scan'")
            .execute(&pool)
            .await?;
        recording.finish().await;
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM scan_jobs")
                .fetch_one(&pool)
                .await?,
            "needs_review"
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn unchanged_or_regressing_ffmpeg_timestamps_do_not_refresh_stalled_eta() -> Result<()> {
        let pool = SqlitePool::connect("sqlite::memory:").await?;
        sqlx::query("CREATE TABLE scan_jobs(id TEXT,progress TEXT)")
            .execute(&pool)
            .await?;
        let recording = Recording::start(&pool, "scan_jobs", "unused", MediaWorkflow::Scan, 0);
        let progress = &recording.progress;
        progress.stage(0, "Decode", Some(100.0), "media seconds");
        progress.ffmpeg_line("out_time_us=50000000");
        {
            let mut current = progress.0.lock().unwrap();
            current.started = Instant::now() - Duration::from_secs(20);
        }
        assert!(progress.snapshot().rate_and_eta().is_some());
        {
            let mut current = progress.0.lock().unwrap();
            current.last_advance = Instant::now() - Duration::from_secs(11);
        }
        for line in [
            "out_time_us=50000000",
            "out_time_us=40000000",
            "out_time_us=50000000",
        ] {
            progress.ffmpeg_line(line);
            assert_eq!(progress.snapshot().fraction(), None);
            assert_eq!(progress.snapshot().rate_and_eta(), None);
        }
        progress.ffmpeg_line("out_time_us=60000000");
        assert_eq!(progress.snapshot().fraction(), Some(0.6));
        assert!(progress.snapshot().rate_and_eta().is_some());
        recording.finish().await;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn snapshots_are_persisted_throttled_and_never_advance_job_state() -> Result<()> {
        let directory = tempfile::tempdir()?;
        wetube_backend::db::initialize(directory.path()).await?;
        let pool = wetube_backend::db::connect(directory.path()).await?;
        sqlx::query("INSERT INTO users VALUES ('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO scan_jobs(id,requested_by,name,source_identity,state,created_at) VALUES ('scan','admin','test.iso','bound-identity','staging',1)").execute(&pool).await?;
        sqlx::query("CREATE TABLE progress_writes (id INTEGER)")
            .execute(&pool)
            .await?;
        sqlx::query("CREATE TRIGGER count_progress AFTER UPDATE OF progress ON scan_jobs BEGIN INSERT INTO progress_writes VALUES (1); END").execute(&pool).await?;
        let recording = Recording::start(&pool, "scan_jobs", "scan", MediaWorkflow::Scan, 0);
        recording.progress.stage(0, "Copy", Some(10000.0), "bytes");
        for done in 0..10000 {
            recording.progress.units(done as f64);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM progress_writes")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            count, 1,
            "per-buffer updates must coalesce into the one-second writer"
        );
        let persisted = wetube_backend::scans::list(&pool).await?.remove(0);
        assert_eq!(persisted.state, wetube_core::ScanState::Staging);
        assert_eq!(persisted.progress.unwrap().completed_units, Some(9999.0));
        recording.progress.stage(2, "Discovering titles", None, "");
        recording.finish().await;
        let persisted = wetube_backend::scans::list(&pool).await?.remove(0);
        let snapshot = persisted.progress.unwrap();
        assert_eq!(snapshot.stage, 2);
        assert_eq!(snapshot.fraction(), None);
        assert_eq!(snapshot.rate_and_eta(), None);
        crate::scans::recover(&pool).await?;
        let interrupted = wetube_backend::scans::list(&pool).await?.remove(0);
        assert_eq!(interrupted.state, wetube_core::ScanState::Failed);
        assert_eq!(
            interrupted.progress,
            Some(snapshot),
            "restart retains the last observation, never resumes its clock"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM movies")
                .fetch_one(&pool)
                .await?,
            0
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn ffmpeg_unknown_negative_or_final_timestamps_do_not_complete_the_stage() -> Result<()> {
        let pool = SqlitePool::connect("sqlite::memory:").await?;
        sqlx::query("CREATE TABLE scan_jobs(id TEXT,progress TEXT)")
            .execute(&pool)
            .await?;
        let recording = Recording::start(&pool, "scan_jobs", "unused", MediaWorkflow::Scan, 0);
        let progress = &recording.progress;
        progress.stage(0, "Decode", Some(100.0), "media seconds");
        progress.ffmpeg_line("out_time_us=N/A");
        progress.ffmpeg_line("out_time_us=-100");
        assert_eq!(progress.snapshot().completed_units, Some(0.0));
        progress.ffmpeg_line("out_time_us=50000000");
        assert_eq!(progress.snapshot().fraction(), Some(0.5));
        progress.ffmpeg_line("out_time_us=100000000");
        progress.ffmpeg_line("progress=end");
        assert_eq!(progress.snapshot().fraction(), Some(0.99));
        assert_eq!(progress.snapshot().stage, 0);
        assert_eq!(progress.snapshot().rate_and_eta(), None);
        {
            let mut current = progress.0.lock().unwrap();
            current.last_advance = Instant::now() - Duration::from_secs(11);
        }
        assert_eq!(
            progress.snapshot().fraction(),
            None,
            "stalled samples have no current throughput/ETA"
        );
        recording.finish().await;
        Ok(())
    }
}

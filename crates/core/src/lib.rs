use serde::{Deserialize, Serialize};

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SCHEMA_VERSION: i64 = 6;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", content = "job", rename_all = "snake_case")]
pub enum ImportJob {
    Scan(ScanJob),
    Probe(ProbeJob),
    Conversion(ConversionJob),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportEntry {
    pub item: ImportJob,
    pub can_archive: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportPage {
    pub entries: Vec<ImportEntry>,
    pub total: u64,
    pub history_count: u64,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArchiveImport {
    pub archived: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaWorkflow {
    Scan,
    Inspect,
    Reinspect,
    Conversion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaProgressState {
    Queued,
    Active,
    Review,
    Complete,
    Failed,
    QueuedPublication,
}

impl MediaWorkflow {
    pub fn stages(self) -> &'static [&'static str] {
        match self {
            Self::Scan => &["Copy input", "Verify input", "Discover DVD"],
            Self::Inspect => &["Copy input", "Verify input", "Inspect program"],
            Self::Reinspect => &["Revalidate input", "Inspect program"],
            Self::Conversion => &[
                "Revalidate input",
                "Decode source",
                "Encode",
                "Validate output",
                "Review",
                "Copy & verify publication",
                "Register",
            ],
        }
    }

    pub fn stage_fills(
        self,
        progress: Option<&MediaProgress>,
        state: MediaProgressState,
    ) -> Vec<Option<f64>> {
        let progress = progress.filter(|p| p.workflow == self && p.stage < self.stages().len());
        let stage = match state {
            MediaProgressState::Complete => self.stages().len(),
            MediaProgressState::Review => 4,
            MediaProgressState::QueuedPublication => 5,
            MediaProgressState::Queued => 0,
            _ => progress.map_or(0, |p| p.stage),
        };
        (0..self.stages().len())
            .map(|index| {
                if index < stage {
                    Some(1.0)
                } else if index == stage
                    && matches!(
                        state,
                        MediaProgressState::Active | MediaProgressState::Failed
                    )
                {
                    progress.and_then(MediaProgress::fraction)
                } else {
                    Some(0.0)
                }
            })
            .collect()
    }
}

/// Observational only: job state and validation, never this snapshot, authorize readiness.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MediaProgress {
    pub workflow: MediaWorkflow,
    pub stage: usize,
    pub phase: String,
    pub elapsed_seconds: f64,
    pub completed_units: Option<f64>,
    pub total_units: Option<f64>,
    pub unit: String,
    pub updated_at: i64,
}

impl MediaProgress {
    pub fn fraction(&self) -> Option<f64> {
        let (done, total) = (self.completed_units?, self.total_units?);
        (done.is_finite() && total.is_finite() && done >= 0.0 && total > 0.0)
            .then(|| (done / total).clamp(0.0, 0.99))
    }

    pub fn rate_and_eta(&self) -> Option<(f64, f64)> {
        let (done, total) = (self.completed_units?, self.total_units?);
        if self.fraction().is_none()
            || !self.elapsed_seconds.is_finite()
            || self.elapsed_seconds < 2.0
            || done <= 0.0
            || done >= total
        {
            return None;
        }
        let rate = done / self.elapsed_seconds;
        let eta = (total - done) / rate;
        (rate.is_finite() && rate > 0.0 && eta.is_finite()).then_some((rate, eta))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    Viewer,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub role: Role,
}

#[derive(Deserialize, Serialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Session {
    pub user: User,
    pub csrf_token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Movie {
    pub id: String,
    pub title: String,
    pub year: Option<i32>,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Library {
    pub movies: Vec<Movie>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MovieDetails {
    pub movie: Movie,
    pub duration_seconds: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlaybackProgress {
    pub position_seconds: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpdatePlaybackProgress {
    pub position_seconds: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SystemStatus {
    pub version: String,
    pub worker_last_seen: Option<i64>,
    pub conversion_enabled: bool,
    pub iso_deletion_enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCheck {
    pub name: String,
    pub available: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiagnosticReport {
    pub tools: Vec<ToolCheck>,
    pub dvdvideo_available: bool,
}

impl DiagnosticReport {
    pub fn required_tools_available(&self) -> bool {
        ["ffmpeg", "ffprobe", "dvdvideo", "DVD discovery"]
            .iter()
            .all(|name| {
                self.tools
                    .iter()
                    .any(|tool| tool.name == *name && tool.available)
            })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiagnosticJob {
    pub id: String,
    pub state: JobState,
    pub created_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
    pub report: Option<DiagnosticReport>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IncomingFile {
    pub name: String,
    pub size_bytes: u64,
    pub identity: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IncomingFiles {
    pub configured: bool,
    pub files: Vec<IncomingFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProbeRequest {
    pub name: String,
    pub identity: String,
    pub title: u8,
    pub angle: u8,
    pub copy_complete: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeState {
    Queued,
    Staging,
    Probing,
    NeedsReview,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProbeJob {
    #[serde(default)]
    pub progress: Option<MediaProgress>,
    pub id: String,
    pub name: String,
    pub title: u8,
    pub angle: u8,
    pub state: ProbeState,
    pub created_at: i64,
    pub attempt_id: Option<String>,
    pub report: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoTreatment {
    Preserve,
    Yadif,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversionState {
    Queued,
    Converting,
    NeedsReview,
    QueuedPublication,
    Publishing,
    Published,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConversionRequest {
    pub probe_job_id: String,
    pub video_treatment: VideoTreatment,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRequest {
    pub title: String,
    pub year: Option<i32>,
    pub description: String,
    pub movie_sha256: String,
    pub confirm: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConversionJob {
    #[serde(default)]
    pub progress: Option<MediaProgress>,
    pub id: String,
    pub probe_job_id: String,
    pub state: ConversionState,
    pub created_at: i64,
    pub attempt_id: Option<String>,
    pub video_treatment: VideoTreatment,
    pub phase: Option<String>,
    pub report: Option<serde_json::Value>,
    pub error: Option<String>,
    pub movie_id: Option<String>,
    pub movie_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConversionJobs {
    pub jobs: Vec<ConversionJob>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScanRequest {
    pub name: String,
    pub identity: String,
    pub copy_complete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {
    pub title: u8,
    pub angle: u8,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanState {
    Queued,
    Staging,
    Scanning,
    NeedsReview,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DvdTitle {
    pub title: u8,
    pub duration_seconds: f64,
    pub chapter_count: u16,
    pub audio_count: u16,
    pub subtitle_count: u16,
    pub audio_languages: Vec<String>,
    pub subtitle_languages: Vec<String>,
    pub angles: Vec<u8>,
    pub main_feature_candidate: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DvdScanReport {
    pub titles: Vec<DvdTitle>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScanJob {
    #[serde(default)]
    pub progress: Option<MediaProgress>,
    pub id: String,
    pub name: String,
    pub state: ScanState,
    pub created_at: i64,
    pub attempt_id: Option<String>,
    pub report: Option<DvdScanReport>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScanJobs {
    pub jobs: Vec<ScanJob>,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
#[error("{0}")]
pub struct ValidationError(pub &'static str);

pub fn normalize_username(username: &str) -> Result<String, ValidationError> {
    let username = username.trim();
    if username.is_empty()
        || username.len() > 64
        || !username
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(ValidationError(
            "Use 1-64 letters, digits, periods, underscores, or hyphens for the username.",
        ));
    }
    Ok(username.to_ascii_lowercase())
}

pub fn validate_password(password: &str) -> Result<(), ValidationError> {
    if password.chars().count() < 12 || password.len() > 1024 {
        return Err(ValidationError(
            "Passwords must contain at least 12 characters and at most 1024 bytes.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media_progress() -> MediaProgress {
        MediaProgress {
            workflow: MediaWorkflow::Conversion,
            stage: 2,
            phase: "Encoding".into(),
            elapsed_seconds: 10.0,
            completed_units: Some(20.0),
            total_units: Some(80.0),
            unit: "media seconds".into(),
            updated_at: 1,
        }
    }

    #[test]
    fn stage_measurements_never_claim_validation_or_an_aggregate_time_percentage() {
        let mut progress = media_progress();
        assert_eq!(progress.fraction(), Some(0.25));
        assert_eq!(progress.rate_and_eta(), Some((2.0, 30.0)));
        let workflow = MediaWorkflow::Conversion;
        assert_eq!(
            workflow.stage_fills(Some(&progress), MediaProgressState::Active),
            vec![
                Some(1.0),
                Some(1.0),
                Some(0.25),
                Some(0.0),
                Some(0.0),
                Some(0.0),
                Some(0.0)
            ]
        );
        progress.completed_units = Some(90.0);
        assert_eq!(progress.fraction(), Some(0.99));
        assert_eq!(progress.rate_and_eta(), None);
        progress.total_units = None;
        assert_eq!(progress.fraction(), None);
        assert_eq!(
            workflow.stage_fills(Some(&progress), MediaProgressState::Active)[2],
            None
        );
        assert_eq!(progress.rate_and_eta(), None);
        for invalid in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            progress.total_units = Some(invalid);
            assert_eq!(progress.fraction(), None);
        }
    }

    #[test]
    fn queued_review_failure_and_terminal_segments_follow_job_state_not_telemetry() {
        let progress = media_progress();
        let workflow = MediaWorkflow::Conversion;
        assert_eq!(
            workflow.stage_fills(Some(&progress), MediaProgressState::Queued),
            vec![Some(0.0); 7]
        );
        let review = workflow.stage_fills(Some(&progress), MediaProgressState::Review);
        assert_eq!(
            review,
            vec![
                Some(1.0),
                Some(1.0),
                Some(1.0),
                Some(1.0),
                Some(0.0),
                Some(0.0),
                Some(0.0)
            ]
        );
        let queued = workflow.stage_fills(Some(&progress), MediaProgressState::QueuedPublication);
        assert_eq!(&queued[..5], &[Some(1.0); 5]);
        assert_eq!(&queued[5..], &[Some(0.0); 2]);
        assert_eq!(
            workflow.stage_fills(Some(&progress), MediaProgressState::Failed),
            workflow.stage_fills(Some(&progress), MediaProgressState::Active)
        );
        assert_eq!(
            workflow.stage_fills(None, MediaProgressState::Complete),
            vec![Some(1.0); 7]
        );
        assert_eq!(
            MediaWorkflow::Scan.stage_fills(None, MediaProgressState::Complete),
            vec![Some(1.0); 3]
        );
        assert_eq!(
            MediaWorkflow::Scan.stage_fills(Some(&progress), MediaProgressState::Active),
            vec![None, Some(0.0), Some(0.0)]
        );
    }

    #[test]
    fn usernames_are_canonical_and_cannot_contain_paths() {
        assert_eq!(normalize_username(" Alice ").unwrap(), "alice");
        for invalid in ["", "../alice", "a/b", "alice@example.com", "a b"] {
            assert!(normalize_username(invalid).is_err());
        }
    }

    #[test]
    fn password_limits_do_not_trim_or_truncate_secrets() {
        assert!(validate_password("short").is_err());
        assert!(validate_password("a sufficiently long password").is_ok());
        assert!(validate_password(&"a".repeat(1025)).is_err());
    }
}

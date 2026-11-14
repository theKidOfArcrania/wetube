use serde::{Deserialize, Serialize};

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SCHEMA_VERSION: i64 = 4;

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

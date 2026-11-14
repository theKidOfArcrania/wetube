pub mod accounts;
pub mod comparisons;
pub mod conversions;
pub mod db;
pub mod import_history;
pub mod incoming;
pub mod labels;
pub mod lock;
pub mod metadata;
pub mod preview;
pub mod probe_jobs;
pub mod scans;
pub mod simple_imports;

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock predates Unix epoch")
        .as_secs() as i64
}
pub mod trash;

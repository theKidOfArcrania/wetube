pub mod accounts;
pub mod conversions;
pub mod db;
pub mod incoming;
pub mod lock;
pub mod preview;
pub mod probe_jobs;
pub mod scans;

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock predates Unix epoch")
        .as_secs() as i64
}

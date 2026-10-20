//! Progress of a download-and-install job (Tailscale or the app), as shown on
//! the status page and returned by the APIs.

use serde::Serialize;

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Progress {
    Idle,
    Checking,
    Downloading { version: String, percent: u8 },
    Installing { version: String },
    Updated { version: String },
    UpToDate { version: String },
    Failed { error: String },
}

impl Progress {
    pub fn busy(&self) -> bool {
        matches!(self, Progress::Checking | Progress::Downloading { .. } | Progress::Installing { .. })
    }
}

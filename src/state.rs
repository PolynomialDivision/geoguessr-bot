use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::{HashMap, VecDeque}, path::Path};

use crate::sources::GeoImage;

/// Ephemeral operational state — persisted across restarts.
/// Analytics live in SQLite (geo.db via db.rs); this only holds what the
/// bot needs to resume without re-fetching everything.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct State {
    /// Pre-fetched images waiting to be used.
    #[serde(default)]
    pub cached_images: VecDeque<GeoImage>,
    /// Set on first boot.
    pub created_at: Option<DateTime<Utc>>,
    /// Per-slot last-fired date. Key = "HH:MM" string from config.
    #[serde(default)]
    pub last_game_dates: HashMap<String, NaiveDate>,
}

impl State {
    pub async fn load(path: &Path) -> Result<Self> {
        if tokio::fs::metadata(path).await.is_ok() {
            let s = tokio::fs::read_to_string(path).await?;
            Ok(serde_json::from_str(&s)?)
        } else {
            Ok(Self::default())
        }
    }

    pub async fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, serde_json::to_string_pretty(self)?).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }
}

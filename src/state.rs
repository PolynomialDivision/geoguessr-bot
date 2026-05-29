use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::{HashMap, VecDeque}, path::Path};

use crate::sources::GeoImage;

// ── Persistent state (operational, not analytics) ─────────────────────────────
//
// Analytics data lives in SQLite (geo.db via db.rs).  This file holds only
// what the bot needs to survive a restart without re-fetching or losing
// in-progress state.

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct State {
    /// Pre-fetched guess data (photo + location) waiting to be used.
    #[serde(default, alias = "cached_images")]
    pub cached_guesses: VecDeque<GeoImage>,
    /// Set on first boot.
    pub created_at: Option<DateTime<Utc>>,
    /// Per-slot last-fired date. Key = "HH:MM" string from config.
    #[serde(default)]
    pub last_game_dates: HashMap<String, NaiveDate>,
    /// Saved when the join-phase message is posted; cleared when the game runs.
    /// Used to reconstruct participants after a restart mid-join-window.
    #[serde(default)]
    pub pending_join: Option<PendingJoin>,
    /// One-time games added via `!schedulegeo`.
    #[serde(default)]
    pub scheduled_once: Vec<ScheduledOnce>,
    /// Per-player language preference (MXID → BCP-47 code, e.g. "en" or "de").
    /// Used for reverse-geocoding guess labels. Defaults to "en" when absent.
    #[serde(default)]
    pub user_langs: HashMap<String, String>,
    /// Runtime overrides for the recurring schedule, set via `!setschedule`.
    #[serde(default)]
    pub schedule_overrides: ScheduleOverrides,
}

/// State saved when the "who wants to play?" message is posted.
/// Persisted so a restart during the join window can resume correctly.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PendingJoin {
    /// Matrix event ID of the join-prompt message (used to re-read reactions).
    pub event_id: String,
    /// The emoji players react with to join.
    pub join_emoji: String,
    /// Slot key ("12:00") or None for one-time / manual games.
    pub slot: Option<String>,
    /// UTC instant when the game images should start being sent.
    pub game_at_utc: DateTime<Utc>,
    /// How long (seconds) players have to submit answers.
    pub answer_timeout_secs: u64,
}

/// A one-time game scheduled via `!schedulegeo`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ScheduledOnce {
    /// Game *start* time as "HH:MM" in the configured timezone.
    pub game_time: String,
    /// Calendar date on which this game should fire.
    pub date: NaiveDate,
    /// Override for how long before game_time the join message fires (seconds).
    /// None → use the value from config.
    #[serde(default)]
    pub reminder_before_secs: Option<u64>,
    /// Override for how long players have to answer (seconds).
    /// None → use the value from config.
    #[serde(default)]
    pub answer_timeout_secs: Option<u64>,
    /// Override for how many guesses (locations) per round.
    /// None → use the value from config or schedule_overrides.
    #[serde(default)]
    pub guesses_per_round: Option<u32>,
}

/// Runtime overrides for the recurring daily schedule, set via `!setschedule`.
/// These sit between per-game overrides (ScheduledOnce) and the static config.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct ScheduleOverrides {
    pub guesses_per_round: Option<u32>,
    pub answer_timeout_secs: Option<u64>,
    pub photos_per_location: Option<usize>,
}

// ── I/O ───────────────────────────────────────────────────────────────────────

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

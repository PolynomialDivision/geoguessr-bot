use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::{HashMap, VecDeque}, path::Path};

use crate::sources::GeoImage;

/// Persisted state for a round that is currently in progress.
/// Saved before each guess so the bot can resume after a restart.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ActiveRoundState {
    pub round_id:            i64,
    pub guess_num:           u32,   // 1-based: the guess currently being played
    pub total_guesses:       u32,
    pub current_image:       GeoImage,
    pub remaining_images:    VecDeque<GeoImage>,  // images not yet started
    pub guess_started_at:    DateTime<Utc>,
    pub answer_timeout_secs: u64,
    pub dm_participants:     HashMap<String, ActiveDmParticipant>,
    pub round_scores:        HashMap<String, i64>,  // accumulated scores so far
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ActiveDmParticipant {
    pub dm_room_id:       String,
    /// Event ID of the "Guess N/N" prompt message posted in this DM room.
    pub prompt_event_id:  Option<String>,
    /// True once the bot has sent "✅ Guess recorded" to this player.
    pub answer_acked:     bool,
}

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
    /// State of the round currently in progress, if any.
    /// Cleared when the round finishes; used to resume after a restart.
    #[serde(default)]
    pub active_round: Option<ActiveRoundState>,
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::GeoImage;

    fn sample_image() -> GeoImage {
        GeoImage {
            country:          "Germany".to_owned(),
            region:           "Europe".to_owned(),
            city:             Some("Munich".to_owned()),
            image_url:        "https://example.com/img.jpg".to_owned(),
            source:           "test".to_owned(),
            attribution:      None,
            lat:              Some(48.1351),
            lon:              Some(11.5820),
            sequence:         None,
            extra_image_urls: vec![],
        }
    }

    fn sample_active_round() -> ActiveRoundState {
        let mut dm = HashMap::new();
        dm.insert("@alice:example.com".to_owned(), ActiveDmParticipant {
            dm_room_id:      "!dm:example.com".to_owned(),
            prompt_event_id: Some("$prompt:example.com".to_owned()),
            answer_acked:    false,
        });
        let mut scores = HashMap::new();
        scores.insert("@alice:example.com".to_owned(), 3500i64);

        ActiveRoundState {
            round_id:            42,
            guess_num:           2,
            total_guesses:       3,
            current_image:       sample_image(),
            remaining_images:    VecDeque::from(vec![sample_image()]),
            guess_started_at:    chrono::DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
                                     .unwrap()
                                     .with_timezone(&Utc),
            answer_timeout_secs: 90,
            dm_participants:     dm,
            round_scores:        scores,
        }
    }

    // ── Serde roundtrips ──────────────────────────────────────────────────────

    #[test]
    fn active_round_state_roundtrip() {
        let original = sample_active_round();
        let json     = serde_json::to_string(&original).unwrap();
        let restored: ActiveRoundState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.round_id, 42);
        assert_eq!(restored.guess_num, 2);
        assert_eq!(restored.total_guesses, 3);
        assert_eq!(restored.answer_timeout_secs, 90);
        assert_eq!(restored.current_image.country, "Germany");
        assert_eq!(restored.remaining_images.len(), 1);

        let alice = restored.dm_participants.get("@alice:example.com").unwrap();
        assert_eq!(alice.dm_room_id, "!dm:example.com");
        assert_eq!(alice.prompt_event_id.as_deref(), Some("$prompt:example.com"));
        assert!(!alice.answer_acked);

        assert_eq!(*restored.round_scores.get("@alice:example.com").unwrap(), 3500);
    }

    #[test]
    fn pending_join_roundtrip() {
        let pj = PendingJoin {
            event_id:            "$join:example.com".to_owned(),
            join_emoji:          "🇬🇧".to_owned(),
            slot:                Some("12:00".to_owned()),
            game_at_utc:         chrono::DateTime::parse_from_rfc3339("2024-06-01T12:00:00Z")
                                     .unwrap()
                                     .with_timezone(&Utc),
            answer_timeout_secs: 120,
        };
        let json     = serde_json::to_string(&pj).unwrap();
        let restored: PendingJoin = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.event_id, "$join:example.com");
        assert_eq!(restored.join_emoji, "🇬🇧");
        assert_eq!(restored.slot.as_deref(), Some("12:00"));
        assert_eq!(restored.answer_timeout_secs, 120);
        assert_eq!(restored.game_at_utc, pj.game_at_utc);
    }

    #[test]
    fn state_roundtrip_with_active_round() {
        let mut state = State::default();
        state.active_round = Some(sample_active_round());
        state.user_langs.insert("@bob:example.com".to_owned(), "de".to_owned());

        let json     = serde_json::to_string(&state).unwrap();
        let restored: State = serde_json::from_str(&json).unwrap();

        assert!(restored.active_round.is_some());
        assert_eq!(restored.active_round.unwrap().round_id, 42);
        assert_eq!(restored.user_langs.get("@bob:example.com").map(|s| s.as_str()), Some("de"));
    }

    #[test]
    fn state_default_roundtrip() {
        let state    = State::default();
        let json     = serde_json::to_string(&state).unwrap();
        let restored: State = serde_json::from_str(&json).unwrap();

        assert!(restored.active_round.is_none());
        assert!(restored.pending_join.is_none());
        assert!(restored.cached_guesses.is_empty());
        assert!(restored.last_game_dates.is_empty());
    }

    #[test]
    fn answer_acked_flag_roundtrip() {
        let p = ActiveDmParticipant {
            dm_room_id:      "!dm:example.com".to_owned(),
            prompt_event_id: None,
            answer_acked:    true,
        };
        let json     = serde_json::to_string(&p).unwrap();
        let restored: ActiveDmParticipant = serde_json::from_str(&json).unwrap();
        assert!(restored.answer_acked);
        assert!(restored.prompt_event_id.is_none());
    }

    // ── Atomic save ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn save_is_atomic_tmp_then_rename() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        let mut state = State::default();
        state.user_langs.insert("@test:example.com".to_owned(), "fr".to_owned());
        state.save(&path).await.unwrap();

        // Rename completed: tmp file must be gone.
        assert!(!path.with_extension("tmp").exists(),
            "tmp file was not cleaned up after save");

        // File must load back to identical state.
        let loaded = State::load(&path).await.unwrap();
        assert_eq!(
            loaded.user_langs.get("@test:example.com").map(|s| s.as_str()),
            Some("fr"),
        );
    }

    #[tokio::test]
    async fn load_returns_default_when_file_missing() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let state = State::load(&path).await.unwrap();
        assert!(state.active_round.is_none());
    }

    #[tokio::test]
    async fn save_then_load_preserves_active_round() {
        let dir   = tempfile::tempdir().unwrap();
        let path  = dir.path().join("state.json");
        let mut state = State::default();
        state.active_round = Some(sample_active_round());
        state.save(&path).await.unwrap();

        let loaded = State::load(&path).await.unwrap();
        let ar = loaded.active_round.unwrap();
        assert_eq!(ar.round_id, 42);
        assert_eq!(ar.guess_num, 2);
        assert_eq!(ar.remaining_images.len(), 1);
        assert_eq!(ar.remaining_images[0].country, "Germany");
    }

    // ── Timeout math ──────────────────────────────────────────────────────────

    #[test]
    fn remaining_timeout_saturates_when_elapsed_exceeds_window() {
        let started      = Utc::now() - chrono::Duration::seconds(200);
        let timeout_secs = 90u64;
        let elapsed      = Utc::now()
            .signed_duration_since(started)
            .num_seconds()
            .max(0) as u64;
        assert_eq!(timeout_secs.saturating_sub(elapsed), 0);
    }

    #[test]
    fn remaining_timeout_when_time_is_left() {
        let started      = Utc::now() - chrono::Duration::seconds(30);
        let timeout_secs = 90u64;
        let elapsed      = Utc::now()
            .signed_duration_since(started)
            .num_seconds()
            .max(0) as u64;
        let remaining    = timeout_secs.saturating_sub(elapsed);
        assert!(remaining > 50 && remaining <= 60,
            "expected ~60s remaining, got {remaining}");
    }
}

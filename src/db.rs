//! SQLite persistence layer (analytics + leaderboard).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use std::{collections::HashMap, path::Path, sync::{Arc, Mutex}};
use tracing::info;

pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

// ── Result types ──────────────────────────────────────────────────────────────

pub struct LeaderboardEntry {
    pub user_id:         String,
    pub total_correct:   i64,
    pub total_questions: i64,
    pub rounds_played:   i64,
}

pub struct ScoreLeaderboardEntry {
    pub user_id:       String,
    pub total_score:   i64,
    pub rounds_played: i64,
}

pub struct UserStatsRow {
    pub total_correct:   i64,
    pub total_questions: i64,
    pub rounds_played:   i64,
}

pub struct CountryStat {
    pub country:    String,
    pub region:     String,
    pub times_asked: i64,
    pub total_answers: i64,
    pub correct_answers: i64,
}

pub struct UserCountryStat {
    pub country:  String,
    pub answered: i64,
    pub correct:  i64,
}

pub struct SpeedEntry {
    pub user_id:      String,
    pub avg_secs:     f64,
    pub sample_count: i64,
}

#[derive(Clone, Debug)]
pub struct AnswerRecord {
    pub choice:         u8,
    pub source:         &'static str,
    pub submitted_at:   DateTime<Utc>,
    pub changed_answer: bool,
}

// ── spawn_blocking helper ─────────────────────────────────────────────────────

impl Db {
    async fn run<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|_| anyhow::anyhow!("DB lock poisoned"))?;
            f(&mut guard)
        })
        .await
        .context("spawn_blocking")?
    }
}

// ── Open + migrate ────────────────────────────────────────────────────────────

impl Db {
    pub async fn open(path: &Path) -> Result<Self> {
        let path = path.to_owned();
        let conn = tokio::task::spawn_blocking(move || {
            Connection::open(&path).context("Opening SQLite database")
        })
        .await
        .context("spawn_blocking open")??;

        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub async fn migrate(&self) -> Result<()> {
        let schema = include_str!("../migrations/schema.sql");
        self.run(move |conn| {
            conn.execute_batch(schema).context("Applying schema")?;
            info!("Database schema OK");
            Ok(())
        })
        .await
    }
}

// ── Round lifecycle ───────────────────────────────────────────────────────────

impl Db {
    pub async fn start_round(
        &self,
        room_id:      &str,
        n_images:     u32,
        triggered_by: &str,
    ) -> Result<i64> {
        let room_id      = room_id.to_owned();
        let triggered_by = triggered_by.to_owned();
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO rounds (room_id, n_images, triggered_by) VALUES (?1, ?2, ?3)",
                params![room_id, n_images, triggered_by],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    pub async fn finish_round(&self, round_id: i64) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "UPDATE rounds SET ended_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?1",
                params![round_id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn round_count(&self) -> Result<i64> {
        self.run(|conn| {
            Ok(conn.query_row("SELECT COUNT(*) FROM rounds", [], |r| r.get(0))?)
        })
        .await
    }
}

// ── Image (question) lifecycle ────────────────────────────────────────────────

impl Db {
    #[allow(clippy::too_many_arguments)]
    pub async fn start_image(
        &self,
        round_id:     i64,
        image_num:    u32,
        country:      &str,
        region:       &str,
        city:         Option<&str>,
        source:       &str,
        attribution:  Option<&str>,
        choices:      &[String],
        correct_idx:  u8,
        timeout_secs: u64,
        actual_lat:   Option<f64>,
        actual_lon:   Option<f64>,
    ) -> Result<i64> {
        let country     = country.to_owned();
        let region      = region.to_owned();
        let city        = city.map(|s| s.to_owned());
        let source      = source.to_owned();
        let attribution = attribution.map(|s| s.to_owned());
        let choices_json = serde_json::to_string(choices)?;
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO images
                   (round_id, image_num, country, region, city, source, attribution,
                    choices, correct_index, answer_timeout_secs, actual_lat, actual_lon)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    round_id, image_num, country, region, city, source, attribution,
                    choices_json, correct_idx as i64, timeout_secs as i64,
                    actual_lat, actual_lon,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    pub async fn set_image_event_id(&self, image_id: i64, event_id: &str) -> Result<()> {
        let event_id = event_id.to_owned();
        self.run(move |conn| {
            conn.execute(
                "UPDATE images SET matrix_event_id = ?1 WHERE id = ?2",
                params![event_id, image_id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn finish_image(
        &self,
        image_id:   i64,
        n_answers:  usize,
        n_correct:  usize,
    ) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "UPDATE images SET n_answers_received=?1, n_correct=?2 WHERE id=?3",
                params![n_answers as i64, n_correct as i64, image_id],
            )?;
            Ok(())
        })
        .await
    }

    /// Check whether a country has been shown in a previous round.
    pub async fn country_asked_before(&self, country: &str) -> Result<bool> {
        let country = country.to_owned();
        self.run(move |conn| {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM images WHERE country = ?1",
                params![country],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
        .await
    }
}

// ── Answer recording ──────────────────────────────────────────────────────────

impl Db {
    pub async fn record_answers(
        &self,
        image_id:   i64,
        round_id:   i64,
        answers:    HashMap<String, AnswerRecord>,
        correct_idx: u8,
    ) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            for (user_id, rec) in &answers {
                let is_correct = if rec.choice == correct_idx { 1i64 } else { 0 };
                tx.execute(
                    "INSERT INTO answers
                       (image_id, round_id, user_id, choice_index, is_correct,
                        source, submitted_at, changed_answer)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                     ON CONFLICT(image_id, user_id) DO UPDATE SET
                       choice_index   = excluded.choice_index,
                       is_correct     = excluded.is_correct,
                       source         = excluded.source,
                       submitted_at   = excluded.submitted_at,
                       changed_answer = excluded.changed_answer",
                    params![
                        image_id, round_id, user_id,
                        rec.choice as i64, is_correct,
                        rec.source, rec.submitted_at.to_rfc3339(),
                        if rec.changed_answer { 1i64 } else { 0 },
                    ],
                )?;

                // Upsert player.
                tx.execute(
                    "INSERT INTO players (user_id) VALUES (?1)
                     ON CONFLICT(user_id) DO UPDATE SET
                       last_seen_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
                    params![user_id],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn upsert_round_scores(
        &self,
        round_id: i64,
        answers_by_user: &HashMap<String, Vec<bool>>,
    ) -> Result<()> {
        let round_id = round_id;
        let data: Vec<(String, i64, i64)> = answers_by_user
            .iter()
            .map(|(uid, results)| {
                let correct = results.iter().filter(|&&c| c).count() as i64;
                let total   = results.len() as i64;
                (uid.clone(), correct, total)
            })
            .collect();
        self.run(move |conn| {
            let tx = conn.transaction()?;
            for (user_id, correct, total) in &data {
                tx.execute(
                    "INSERT INTO round_scores (round_id, user_id, correct_count, total_count)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(round_id, user_id) DO UPDATE SET
                       correct_count = correct_count + excluded.correct_count,
                       total_count   = total_count   + excluded.total_count",
                    params![round_id, user_id, correct, total],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Record free-guess answers (distance + score instead of correct/wrong).
    pub async fn record_free_guess_answers(
        &self,
        image_id:  i64,
        round_id:  i64,
        guesses:   Vec<(String, String, f64, f64, f64, i64)>,
        // (user_id, guess_text, guess_lat, guess_lon, distance_km, score)
    ) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            for (user_id, guess_text, guess_lat, guess_lon, distance_km, score) in &guesses {
                tx.execute(
                    "INSERT INTO answers
                       (image_id, round_id, user_id, choice_index, is_correct, source,
                        guess_text, guess_lat, guess_lon, distance_km, score)
                     VALUES (?1,?2,?3,0,0,'free_guess',?4,?5,?6,?7,?8)
                     ON CONFLICT(image_id, user_id) DO UPDATE SET
                       guess_text   = excluded.guess_text,
                       guess_lat    = excluded.guess_lat,
                       guess_lon    = excluded.guess_lon,
                       distance_km  = excluded.distance_km,
                       score        = excluded.score",
                    params![image_id, round_id, user_id, guess_text, guess_lat, guess_lon, distance_km, score],
                )?;
                tx.execute(
                    "INSERT INTO players (user_id) VALUES (?1)
                     ON CONFLICT(user_id) DO UPDATE SET
                       last_seen_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
                    params![user_id],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Upsert round scores for free-guess mode (total_score column).
    pub async fn upsert_round_scores_free_guess(
        &self,
        round_id: i64,
        scores_by_user: &HashMap<String, i64>,
    ) -> Result<()> {
        let data: Vec<(String, i64)> = scores_by_user
            .iter()
            .map(|(uid, &score)| (uid.clone(), score))
            .collect();
        self.run(move |conn| {
            let tx = conn.transaction()?;
            for (user_id, score) in &data {
                tx.execute(
                    "INSERT INTO round_scores (round_id, user_id, correct_count, total_count, total_score)
                     VALUES (?1, ?2, 0, 1, ?3)
                     ON CONFLICT(round_id, user_id) DO UPDATE SET
                       total_count  = total_count  + 1,
                       total_score  = total_score  + excluded.total_score",
                    params![round_id, user_id, score],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Leaderboard ranked by total GeoGuessr score (free-guess mode).
    pub async fn score_leaderboard(&self) -> Result<Vec<ScoreLeaderboardEntry>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT user_id,
                        SUM(total_score) AS total_score,
                        COUNT(*)         AS rounds_played
                   FROM round_scores
                  GROUP BY user_id
                  ORDER BY total_score DESC",
            )?;
            let rows = stmt.query_map([], |r| Ok(ScoreLeaderboardEntry {
                user_id:      r.get(0)?,
                total_score:  r.get(1)?,
                rounds_played: r.get(2)?,
            }))?;
            rows.map(|r| r.context("reading score leaderboard row")).collect()
        })
        .await
    }
}

// ── Leaderboard + stats ───────────────────────────────────────────────────────

impl Db {
    pub async fn leaderboard(&self) -> Result<Vec<LeaderboardEntry>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT user_id,
                        SUM(correct_count) AS total_correct,
                        SUM(total_count)   AS total_questions,
                        COUNT(*)           AS rounds_played
                   FROM round_scores
                  GROUP BY user_id
                  ORDER BY total_correct DESC, total_questions ASC",
            )?;
            let rows = stmt.query_map([], |r| Ok(LeaderboardEntry {
                user_id:         r.get(0)?,
                total_correct:   r.get(1)?,
                total_questions: r.get(2)?,
                rounds_played:   r.get(3)?,
            }))?;
            rows.map(|r| r.context("reading leaderboard row")).collect()
        })
        .await
    }

    pub async fn user_stats(&self, user_id: &str) -> Result<Option<UserStatsRow>> {
        let user_id = user_id.to_owned();
        self.run(move |conn| {
            let res = conn.query_row(
                "SELECT SUM(correct_count), SUM(total_count), COUNT(*)
                   FROM round_scores WHERE user_id = ?1",
                params![user_id],
                |r| Ok((r.get::<_,Option<i64>>(0)?, r.get::<_,Option<i64>>(1)?, r.get::<_,i64>(2)?)),
            )?;
            if res.2 == 0 { return Ok(None); }
            Ok(Some(UserStatsRow {
                total_correct:   res.0.unwrap_or(0),
                total_questions: res.1.unwrap_or(0),
                rounds_played:   res.2,
            }))
        })
        .await
    }

    pub async fn country_stats(&self) -> Result<Vec<CountryStat>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT i.country, i.region,
                        COUNT(DISTINCT i.id)   AS times_asked,
                        COUNT(a.id)            AS total_answers,
                        SUM(a.is_correct)      AS correct_answers
                   FROM images i
                   LEFT JOIN answers a ON a.image_id = i.id
                  GROUP BY i.country
                  ORDER BY times_asked DESC",
            )?;
            let rows = stmt.query_map([], |r| Ok(CountryStat {
                country:         r.get(0)?,
                region:          r.get(1)?,
                times_asked:     r.get(2)?,
                total_answers:   r.get(3)?,
                correct_answers: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            }))?;
            rows.map(|r| r.context("reading country stat row")).collect()
        })
        .await
    }

    pub async fn user_country_stats(&self, user_id: &str) -> Result<Vec<UserCountryStat>> {
        let user_id = user_id.to_owned();
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT i.country, COUNT(*) AS answered, SUM(a.is_correct) AS correct
                   FROM answers a
                   JOIN images i ON i.id = a.image_id
                  WHERE a.user_id = ?1
                  GROUP BY i.country
                  ORDER BY (CAST(SUM(a.is_correct) AS REAL) / COUNT(*)) DESC",
            )?;
            let rows = stmt.query_map(params![user_id], |r| Ok(UserCountryStat {
                country:  r.get(0)?,
                answered: r.get(1)?,
                correct:  r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            }))?;
            rows.map(|r| r.context("reading user country stat row")).collect()
        })
        .await
    }

    pub async fn speed_leaderboard(&self) -> Result<Vec<SpeedEntry>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT a.user_id,
                        AVG(CAST(
                            (julianday(a.submitted_at) - julianday(i.asked_at))
                            * 86400.0 AS REAL
                        )) AS avg_secs,
                        COUNT(*) AS sample_count
                   FROM answers a
                   JOIN images i ON i.id = a.image_id
                  WHERE a.is_correct = 1
                  GROUP BY a.user_id
                 HAVING COUNT(*) >= 3
                  ORDER BY avg_secs ASC",
            )?;
            let rows = stmt.query_map([], |r| Ok(SpeedEntry {
                user_id:      r.get(0)?,
                avg_secs:     r.get(1)?,
                sample_count: r.get(2)?,
            }))?;
            rows.map(|r| r.context("reading speed row")).collect()
        })
        .await
    }

    pub async fn reset_stats(&self) -> Result<()> {
        self.run(|conn| {
            conn.execute_batch(
                "DELETE FROM answers;
                 DELETE FROM round_scores;
                 DELETE FROM images;
                 DELETE FROM rounds;
                 DELETE FROM players;",
            )?;
            Ok(())
        })
        .await
    }
}

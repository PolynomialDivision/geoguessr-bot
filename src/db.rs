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

pub struct RoundStatsEntry {
    pub user_id:          String,
    pub total_score:      i64,
    pub guesses_played:   i64,
    pub avg_distance_km:  f64,
    pub best_distance_km: f64,
}

pub struct ScoreLeaderboardEntry {
    pub user_id:          String,
    pub total_score:      i64,
    pub rounds_played:    i64,
    /// Total guesses counted toward this player: real submissions PLUS
    /// missed/no-guess rounds recorded as 0 (see `Db::record_missed_guesses`).
    /// This is the denominator for both the raw average and the leaderboard
    /// rating, so a player can't inflate either by skipping hard guesses.
    pub guesses_played:   i64,
    /// Of `guesses_played`, how many were real player submissions (i.e.
    /// `guesses_played - guesses_answered` is the missed count).
    pub guesses_answered: i64,
    /// Average Haversine distance across real (non-missed) guesses (km).
    pub avg_distance_km:  f64,
    /// Best (closest) single guess ever (km).
    pub best_distance_km: f64,
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
        // Step 1: rename old tables/columns if the DB was created before the
        //         "images → guesses" rename.  All ALTER TABLE ops are idempotent:
        //         we check sqlite_master first, so they only run once.
        self.run(|conn| {
            let has_images: bool = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='images'",
                [],
                |r| r.get::<_, i64>(0),
            ).unwrap_or(0) > 0;

            if has_images {
                info!("DB migration: renaming 'images' table → 'guesses'");
                conn.execute_batch("
                    ALTER TABLE images  RENAME TO guesses;
                    ALTER TABLE guesses RENAME COLUMN image_num  TO guess_num;
                    ALTER TABLE answers RENAME COLUMN image_id   TO guess_id;
                    ALTER TABLE rounds  RENAME COLUMN n_images   TO n_guesses;
                ")?;
            }
            Ok(())
        })
        .await?;

        // Step 2: apply the current schema (CREATE TABLE IF NOT EXISTS — safe to re-run).
        let schema = include_str!("../migrations/schema.sql");
        self.run(move |conn| {
            conn.execute_batch(schema).context("Applying schema")?;
            info!("Database schema OK");
            Ok(())
        })
        .await?;

        // Step 3: add `answers.missed` for databases created before it existed.
        // `CREATE TABLE IF NOT EXISTS` in step 2 doesn't retroactively add
        // columns to an already-existing table, so existing installs need an
        // explicit ALTER. Checked via PRAGMA table_info rather than catching
        // the "duplicate column" error, so this doesn't depend on how a given
        // SQLite/rusqlite version reports that error.
        self.run(|conn| {
            let has_missed_col = conn
                .prepare("PRAGMA table_info(answers)")?
                .query_map([], |r| r.get::<_, String>(1))?
                .filter_map(|r| r.ok())
                .any(|name| name == "missed");
            if !has_missed_col {
                info!("DB migration: adding answers.missed column");
                conn.execute(
                    "ALTER TABLE answers ADD COLUMN missed INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
            }
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
        n_guesses:    u32,
        triggered_by: &str,
    ) -> Result<i64> {
        let room_id      = room_id.to_owned();
        let triggered_by = triggered_by.to_owned();
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO rounds (room_id, n_guesses, triggered_by) VALUES (?1, ?2, ?3)",
                params![room_id, n_guesses, triggered_by],
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

// ── Guess lifecycle ───────────────────────────────────────────────────────────

impl Db {
    #[allow(clippy::too_many_arguments)]
    pub async fn start_guess(
        &self,
        round_id:     i64,
        guess_num:    u32,
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
                "INSERT INTO guesses
                   (round_id, guess_num, country, region, city, source, attribution,
                    choices, correct_index, answer_timeout_secs, actual_lat, actual_lon)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    round_id, guess_num, country, region, city, source, attribution,
                    choices_json, correct_idx as i64, timeout_secs as i64,
                    actual_lat, actual_lon,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    pub async fn set_guess_event_id(&self, guess_id: i64, event_id: &str) -> Result<()> {
        let event_id = event_id.to_owned();
        self.run(move |conn| {
            conn.execute(
                "UPDATE guesses SET matrix_event_id = ?1 WHERE id = ?2",
                params![event_id, guess_id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn finish_guess(
        &self,
        guess_id:  i64,
        n_answers: usize,
        n_correct: usize,
    ) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "UPDATE guesses SET n_answers_received=?1, n_correct=?2 WHERE id=?3",
                params![n_answers as i64, n_correct as i64, guess_id],
            )?;
            Ok(())
        })
        .await
    }

    /// Return the `id` of an existing guess row for (round_id, guess_num), if any.
    /// Used on restart to retrieve the existing id rather than inserting a duplicate.
    pub async fn find_guess_id(&self, round_id: i64, guess_num: u32) -> Option<i64> {
        let guess_num = guess_num as i64;
        self.run(move |conn| {
            match conn.query_row(
                "SELECT id FROM guesses WHERE round_id = ?1 AND guess_num = ?2 LIMIT 1",
                params![round_id, guess_num],
                |r| r.get::<_, i64>(0),
            ) {
                Ok(id)                                    => Ok(Some(id)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e)                                    => Err(e.into()),
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// Check whether a country has been shown in a previous round.
    pub async fn country_asked_before(&self, country: &str) -> Result<bool> {
        let country = country.to_owned();
        self.run(move |conn| {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM guesses WHERE country = ?1",
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
        guess_id:   i64,
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
                       (guess_id, round_id, user_id, choice_index, is_correct,
                        source, submitted_at, changed_answer)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                     ON CONFLICT(guess_id, user_id) DO UPDATE SET
                       choice_index   = excluded.choice_index,
                       is_correct     = excluded.is_correct,
                       source         = excluded.source,
                       submitted_at   = excluded.submitted_at,
                       changed_answer = excluded.changed_answer",
                    params![
                        guess_id, round_id, user_id,
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
        guess_id:  i64,
        round_id:  i64,
        guesses:   Vec<(String, String, f64, f64, f64, i64)>,
        // (user_id, guess_text, guess_lat, guess_lon, distance_km, score)
    ) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            for (user_id, guess_text, guess_lat, guess_lon, distance_km, score) in &guesses {
                tx.execute(
                    "INSERT INTO answers
                       (guess_id, round_id, user_id, choice_index, is_correct, source,
                        guess_text, guess_lat, guess_lon, distance_km, score)
                     VALUES (?1,?2,?3,0,0,'free_guess',?4,?5,?6,?7,?8)
                     ON CONFLICT(guess_id, user_id) DO UPDATE SET
                       guess_text   = excluded.guess_text,
                       guess_lat    = excluded.guess_lat,
                       guess_lon    = excluded.guess_lon,
                       distance_km  = excluded.distance_km,
                       score        = excluded.score,
                       missed       = 0",
                    params![guess_id, round_id, user_id, guess_text, guess_lat, guess_lon, distance_km, score],
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

    /// Record that `user_ids` were part of this guess's roster (join-phase
    /// participants, or players who already answered earlier in the round)
    /// but did not submit anything. Inserted as `score = 0, missed = 1` rows
    /// so every joined round counts toward a player's average and stats —
    /// skipping a guess can no longer simply leave no trace.
    ///
    /// Uses `DO NOTHING` on conflict: a real answer (from
    /// `record_free_guess_answers`) always takes precedence and is never
    /// clobbered by a missed-guess record for the same (guess, user).
    pub async fn record_missed_guesses(
        &self,
        guess_id: i64,
        round_id: i64,
        user_ids: &[String],
    ) -> Result<()> {
        if user_ids.is_empty() {
            return Ok(());
        }
        let user_ids = user_ids.to_vec();
        self.run(move |conn| {
            let tx = conn.transaction()?;
            for user_id in &user_ids {
                tx.execute(
                    "INSERT INTO answers
                       (guess_id, round_id, user_id, choice_index, is_correct,
                        source, score, missed)
                     VALUES (?1, ?2, ?3, 0, 0, 'missed', 0, 1)
                     ON CONFLICT(guess_id, user_id) DO NOTHING",
                    params![guess_id, round_id, user_id],
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
    /// Also returns avg distance and images played per user.
    pub async fn round_stats(&self, round_id: i64) -> Result<Vec<RoundStatsEntry>> {
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT user_id,
                        SUM(score)         AS total_score,
                        COUNT(*)           AS guesses_played,
                        AVG(distance_km)   AS avg_distance_km,
                        MIN(distance_km)   AS best_distance_km
                   FROM answers
                  WHERE round_id = ?1
                    AND distance_km IS NOT NULL
                  GROUP BY user_id
                  ORDER BY total_score DESC",
            )?;
            let rows = stmt.query_map([round_id], |r| Ok(RoundStatsEntry {
                user_id:          r.get(0)?,
                total_score:      r.get(1)?,
                guesses_played:   r.get(2)?,
                avg_distance_km:  r.get(3)?,
                best_distance_km: r.get(4)?,
            }))?;
            rows.map(|r| r.context("reading round stats row")).collect()
        })
        .await
    }

    /// All-time leaderboard — includes every round ever played.
    ///
    /// `guesses_played` counts every answer row (real + missed) so a player's
    /// average and rating reflect every guess they were on the hook for, not
    /// just the ones they chose to submit. `guesses_answered` (real
    /// submissions only) is kept separately for completion-rate display, and
    /// distance stats are computed only from real (non-missed) rows.
    pub async fn score_leaderboard_alltime(&self) -> Result<Vec<ScoreLeaderboardEntry>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT rs.user_id,
                        SUM(rs.total_score)                AS total_score,
                        COUNT(DISTINCT rs.round_id)        AS rounds_played,
                        COALESCE(a.guesses_played, 0)      AS guesses_played,
                        COALESCE(a.guesses_answered, 0)    AS guesses_answered,
                        COALESCE(a.avg_distance_km, 0.0)   AS avg_distance_km,
                        COALESCE(a.best_distance_km, 0.0)  AS best_distance_km
                   FROM round_scores rs
                   LEFT JOIN (
                       SELECT user_id,
                              COUNT(*) AS guesses_played,
                              SUM(CASE WHEN missed = 0 THEN 1 ELSE 0 END) AS guesses_answered,
                              AVG(CASE WHEN missed = 0 THEN distance_km END) AS avg_distance_km,
                              MIN(CASE WHEN missed = 0 THEN distance_km END) AS best_distance_km
                         FROM answers
                        GROUP BY user_id
                   ) a ON a.user_id = rs.user_id
                  GROUP BY rs.user_id",
            )?;
            let rows = stmt.query_map([], |r| Ok(ScoreLeaderboardEntry {
                user_id:          r.get(0)?,
                total_score:      r.get(1)?,
                rounds_played:    r.get(2)?,
                guesses_played:   r.get(3)?,
                guesses_answered: r.get(4)?,
                avg_distance_km:  r.get(5)?,
                best_distance_km: r.get(6)?,
            }))?;
            rows.map(|r| r.context("reading score leaderboard row")).collect()
        })
        .await
    }

    /// Rolling leaderboard — only rounds from the last 90 days.
    /// Same real-vs-missed split as `score_leaderboard_alltime`; see there.
    pub async fn score_leaderboard(&self) -> Result<Vec<ScoreLeaderboardEntry>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT rs.user_id,
                        SUM(rs.total_score)                AS total_score,
                        COUNT(DISTINCT rs.round_id)        AS rounds_played,
                        COALESCE(a.guesses_played, 0)      AS guesses_played,
                        COALESCE(a.guesses_answered, 0)    AS guesses_answered,
                        COALESCE(a.avg_distance_km, 0.0)   AS avg_distance_km,
                        COALESCE(a.best_distance_km, 0.0)  AS best_distance_km
                   FROM round_scores rs
                   JOIN rounds ro ON ro.id = rs.round_id
                    AND ro.started_at >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-90 days')
                   LEFT JOIN (
                       SELECT ans.user_id,
                              COUNT(*) AS guesses_played,
                              SUM(CASE WHEN ans.missed = 0 THEN 1 ELSE 0 END) AS guesses_answered,
                              AVG(CASE WHEN ans.missed = 0 THEN ans.distance_km END) AS avg_distance_km,
                              MIN(CASE WHEN ans.missed = 0 THEN ans.distance_km END) AS best_distance_km
                         FROM answers ans
                         JOIN guesses g  ON g.id  = ans.guess_id
                         JOIN rounds  ri ON ri.id = g.round_id
                        WHERE ri.started_at >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-90 days')
                        GROUP BY ans.user_id
                   ) a ON a.user_id = rs.user_id
                  GROUP BY rs.user_id",
            )?;
            let rows = stmt.query_map([], |r| Ok(ScoreLeaderboardEntry {
                user_id:          r.get(0)?,
                total_score:      r.get(1)?,
                rounds_played:    r.get(2)?,
                guesses_played:   r.get(3)?,
                guesses_answered: r.get(4)?,
                avg_distance_km:  r.get(5)?,
                best_distance_km: r.get(6)?,
            }))?;
            rows.map(|r| r.context("reading score leaderboard row")).collect()
        })
        .await
    }
}

// ── Location history (dedup) ──────────────────────────────────────────────────

impl Db {
    /// Return the coordinates of all played locations so the prefetcher can
    /// exclude nearby areas (within MIN_DISTANCE_KM).
    pub async fn recent_played_coords(&self) -> Result<Vec<(f64, f64)>> {
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT actual_lat, actual_lon FROM guesses
                 WHERE actual_lat IS NOT NULL
                   AND actual_lon IS NOT NULL
                   AND asked_at >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-90 days')",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, f64>(0)?, row.get::<_, f64>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .context("reading played coords")
        })
        .await
    }

    /// Returns how many times each country was played in the last 90 days.
    pub async fn recent_country_counts(&self) -> Result<HashMap<String, u32>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT country, COUNT(*) AS cnt
                   FROM guesses
                  WHERE asked_at >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-90 days')
                  GROUP BY country",
            )?;
            let pairs: Vec<(String, u32)> = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("reading country counts")?;
            Ok(pairs.into_iter().collect())
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
                "SELECT g.country, g.region,
                        COUNT(DISTINCT g.id)   AS times_asked,
                        COUNT(a.id)            AS total_answers,
                        SUM(a.is_correct)      AS correct_answers
                   FROM guesses g
                   LEFT JOIN answers a ON a.guess_id = g.id
                  GROUP BY g.country
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
                "SELECT g.country, COUNT(*) AS answered, SUM(a.is_correct) AS correct
                   FROM answers a
                   JOIN guesses g ON g.id = a.guess_id
                  WHERE a.user_id = ?1
                  GROUP BY g.country
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
                            (julianday(a.submitted_at) - julianday(g.asked_at))
                            * 86400.0 AS REAL
                        )) AS avg_secs,
                        COUNT(*) AS sample_count
                   FROM answers a
                   JOIN guesses g ON g.id = a.guess_id
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
                 DELETE FROM guesses;
                 DELETE FROM rounds;
                 DELETE FROM players;",
            )?;
            Ok(())
        })
        .await
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an in-memory Db with the current schema applied.
    async fn mem_db() -> Db {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(include_str!("../migrations/schema.sql"))
            .expect("schema");
        Db { conn: Arc::new(Mutex::new(conn)) }
    }

    // ── find_guess_id ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn find_guess_id_returns_none_when_empty() {
        let db = mem_db().await;
        assert!(db.find_guess_id(999, 1).await.is_none());
    }

    #[tokio::test]
    async fn find_guess_id_returns_existing_row() {
        let db = mem_db().await;
        let round_id = db.start_round("!room:example.com", 3, "test").await.unwrap();
        let guess_id = db.start_guess(
            round_id, 1, "Germany", "Europe", Some("Berlin"),
            "test", None, &[], 0, 90, Some(52.52), Some(13.40),
        ).await.unwrap();

        let found = db.find_guess_id(round_id, 1).await;
        assert_eq!(found, Some(guess_id));
    }

    #[tokio::test]
    async fn find_guess_id_wrong_guess_num_returns_none() {
        let db = mem_db().await;
        let round_id = db.start_round("!room:example.com", 3, "test").await.unwrap();
        db.start_guess(round_id, 1, "Germany", "Europe", None, "test", None, &[], 0, 90, None, None).await.unwrap();

        assert!(db.find_guess_id(round_id, 2).await.is_none());
    }

    #[tokio::test]
    async fn find_guess_id_wrong_round_returns_none() {
        let db = mem_db().await;
        let round_id = db.start_round("!room:example.com", 3, "test").await.unwrap();
        db.start_guess(round_id, 1, "Germany", "Europe", None, "test", None, &[], 0, 90, None, None).await.unwrap();

        assert!(db.find_guess_id(round_id + 1, 1).await.is_none());
    }

    #[tokio::test]
    async fn find_guess_id_multiple_guesses_in_round() {
        let db = mem_db().await;
        let round_id = db.start_round("!room:example.com", 3, "test").await.unwrap();
        let g1 = db.start_guess(round_id, 1, "France", "Europe", None, "test", None, &[], 0, 90, None, None).await.unwrap();
        let g2 = db.start_guess(round_id, 2, "Japan",  "Asia",   None, "test", None, &[], 0, 90, None, None).await.unwrap();

        assert_eq!(db.find_guess_id(round_id, 1).await, Some(g1));
        assert_eq!(db.find_guess_id(round_id, 2).await, Some(g2));
        assert!(db.find_guess_id(round_id, 3).await.is_none());
    }

    // ── Round lifecycle ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn start_and_finish_round() {
        let db = mem_db().await;
        let round_id = db.start_round("!r:example.com", 2, "manual").await.unwrap();
        assert!(round_id > 0);
        db.finish_round(round_id).await.unwrap();
        // finish_round is idempotent (just sets ended_at).
        db.finish_round(round_id).await.unwrap();
    }

    #[tokio::test]
    async fn finish_guess_updates_answer_count() {
        let db = mem_db().await;
        let round_id = db.start_round("!r:example.com", 1, "test").await.unwrap();
        let guess_id = db.start_guess(round_id, 1, "Brazil", "S. America", None, "test", None, &[], 0, 90, None, None).await.unwrap();
        db.finish_guess(guess_id, 3, 1).await.unwrap();
        // Verify via round_stats (indirect).
        let stats = db.round_stats(round_id).await.unwrap();
        assert!(stats.is_empty()); // no answer rows → no stats rows
    }

    // ── Missed guesses ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn record_missed_guesses_counts_as_zero_in_leaderboard() {
        let db = mem_db().await;
        let round_id = db.start_round("!r:example.com", 1, "test").await.unwrap();
        let guess_id = db
            .start_guess(round_id, 1, "France", "Europe", None, "test", None, &[], 0, 90, Some(48.85), Some(2.35))
            .await
            .unwrap();

        // @alice answers for real; @bob is on the roster but never guesses.
        db.record_free_guess_answers(
            guess_id,
            round_id,
            vec![("@alice:example.com".to_owned(), "Paris".to_owned(), 48.85, 2.35, 0.0, 5000)],
        )
        .await
        .unwrap();
        db.record_missed_guesses(guess_id, round_id, &["@bob:example.com".to_owned()])
            .await
            .unwrap();

        let mut scores = HashMap::new();
        scores.insert("@alice:example.com".to_owned(), 5000i64);
        scores.insert("@bob:example.com".to_owned(), 0i64);
        db.upsert_round_scores_free_guess(round_id, &scores).await.unwrap();

        let board = db.score_leaderboard_alltime().await.unwrap();
        let alice = board.iter().find(|e| e.user_id == "@alice:example.com").unwrap();
        let bob = board.iter().find(|e| e.user_id == "@bob:example.com").unwrap();

        assert_eq!(alice.guesses_played, 1);
        assert_eq!(alice.guesses_answered, 1);
        assert_eq!(alice.total_score, 5000);

        // The missed guess still counts: it shows up as a played (but not
        // answered) guess with 0 score, and the round still counts for @bob.
        assert_eq!(bob.guesses_played, 1);
        assert_eq!(bob.guesses_answered, 0);
        assert_eq!(bob.total_score, 0);
        assert_eq!(bob.rounds_played, 1);
    }

    #[tokio::test]
    async fn record_missed_guesses_does_not_clobber_a_real_answer() {
        let db = mem_db().await;
        let round_id = db.start_round("!r:example.com", 1, "test").await.unwrap();
        let guess_id = db
            .start_guess(round_id, 1, "Japan", "Asia", None, "test", None, &[], 0, 90, None, None)
            .await
            .unwrap();

        db.record_free_guess_answers(
            guess_id,
            round_id,
            vec![("@late:example.com".to_owned(), "Tokyo".to_owned(), 35.6, 139.7, 12.3, 4800)],
        )
        .await
        .unwrap();
        // Defensive: a real answer must win even if a miss is also recorded
        // for the same (guess, user) — DO NOTHING on conflict.
        db.record_missed_guesses(guess_id, round_id, &["@late:example.com".to_owned()])
            .await
            .unwrap();

        let mut scores = HashMap::new();
        scores.insert("@late:example.com".to_owned(), 4800i64);
        db.upsert_round_scores_free_guess(round_id, &scores).await.unwrap();

        let board = db.score_leaderboard_alltime().await.unwrap();
        let late = board.iter().find(|e| e.user_id == "@late:example.com").unwrap();
        assert_eq!(late.guesses_answered, 1);
        assert_eq!(late.total_score, 4800);
    }

    #[tokio::test]
    async fn record_missed_guesses_no_op_on_empty_input() {
        let db = mem_db().await;
        let round_id = db.start_round("!r:example.com", 1, "test").await.unwrap();
        let guess_id = db
            .start_guess(round_id, 1, "Peru", "S. America", None, "test", None, &[], 0, 90, None, None)
            .await
            .unwrap();
        db.record_missed_guesses(guess_id, round_id, &[]).await.unwrap();
    }

    // ── Migration: existing data keeps working ──────────────────────────────

    /// Simulates a database created before the `answers.missed` column
    /// existed, then runs the real `migrate()` and checks old data survives
    /// and the leaderboard query (which now depends on `missed`) still works.
    #[tokio::test]
    async fn migration_adds_missed_column_to_pre_existing_database() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE players (
                user_id      TEXT PRIMARY KEY,
                first_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                last_seen_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );
            CREATE TABLE rounds (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                room_id      TEXT NOT NULL,
                n_guesses    INTEGER NOT NULL,
                triggered_by TEXT NOT NULL,
                started_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                ended_at     TEXT
            );
            CREATE TABLE guesses (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                round_id            INTEGER NOT NULL REFERENCES rounds(id),
                guess_num           INTEGER NOT NULL,
                country             TEXT NOT NULL,
                region              TEXT NOT NULL,
                city                TEXT,
                source              TEXT NOT NULL,
                attribution         TEXT,
                choices             TEXT NOT NULL DEFAULT '[]',
                correct_index       INTEGER NOT NULL DEFAULT 0,
                answer_timeout_secs INTEGER NOT NULL DEFAULT 90,
                actual_lat          REAL,
                actual_lon          REAL,
                matrix_event_id     TEXT,
                asked_at            TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                n_answers_received  INTEGER,
                n_correct           INTEGER
            );
            CREATE TABLE answers (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                guess_id      INTEGER NOT NULL REFERENCES guesses(id),
                round_id      INTEGER NOT NULL REFERENCES rounds(id),
                user_id       TEXT NOT NULL,
                choice_index  INTEGER NOT NULL DEFAULT 0,
                is_correct    INTEGER NOT NULL DEFAULT 0,
                source        TEXT NOT NULL DEFAULT 'reaction',
                submitted_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                changed_answer INTEGER NOT NULL DEFAULT 0,
                guess_text    TEXT,
                guess_lat     REAL,
                guess_lon     REAL,
                distance_km   REAL,
                score         INTEGER,
                UNIQUE(guess_id, user_id)
            );
            CREATE TABLE round_scores (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                round_id      INTEGER NOT NULL REFERENCES rounds(id),
                user_id       TEXT NOT NULL,
                correct_count INTEGER NOT NULL DEFAULT 0,
                total_count   INTEGER NOT NULL DEFAULT 0,
                total_score   INTEGER NOT NULL DEFAULT 0,
                UNIQUE(round_id, user_id)
            );
            INSERT INTO rounds (id, room_id, n_guesses, triggered_by)
                VALUES (1, '!legacy:example.com', 1, 'test');
            INSERT INTO guesses (id, round_id, guess_num, country, region, source)
                VALUES (1, 1, 1, 'Spain', 'Europe', 'test');
            INSERT INTO answers (guess_id, round_id, user_id, guess_text, guess_lat, guess_lon, distance_km, score)
                VALUES (1, 1, '@old-timer:example.com', 'Madrid', 40.4, -3.7, 5.0, 4900);
            INSERT INTO round_scores (round_id, user_id, total_count, total_score)
                VALUES (1, '@old-timer:example.com', 1, 4900);
            ",
        )
        .unwrap();
        let db = Db { conn: Arc::new(Mutex::new(conn)) };

        db.migrate().await.unwrap();

        // Old data survives, defaults to missed=0, and is fully queryable
        // through the leaderboard query that now depends on that column.
        let board = db.score_leaderboard_alltime().await.unwrap();
        let entry = board.iter().find(|e| e.user_id == "@old-timer:example.com").unwrap();
        assert_eq!(entry.total_score, 4900);
        assert_eq!(entry.guesses_played, 1);
        assert_eq!(entry.guesses_answered, 1);
        assert_eq!(entry.rounds_played, 1);

        // And the new missed-guess write path works on the migrated table.
        let guess_id = db.find_guess_id(1, 1).await.unwrap();
        db.record_missed_guesses(guess_id, 1, &["@newcomer:example.com".to_owned()])
            .await
            .unwrap();
    }
}

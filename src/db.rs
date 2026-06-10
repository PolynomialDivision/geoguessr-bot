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
    /// Number of individual guesses submitted (one per puzzle in a round).
    pub guesses_played:   i64,
    /// Average Haversine distance across all guesses (km).
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
                       score        = excluded.score",
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
    pub async fn score_leaderboard_alltime(&self) -> Result<Vec<ScoreLeaderboardEntry>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT rs.user_id,
                        SUM(rs.total_score)                AS total_score,
                        COUNT(DISTINCT rs.round_id)        AS rounds_played,
                        COALESCE(a.guesses_played, 0)      AS guesses_played,
                        COALESCE(a.avg_distance_km, 0.0)   AS avg_distance_km,
                        COALESCE(a.best_distance_km, 0.0)  AS best_distance_km
                   FROM round_scores rs
                   LEFT JOIN (
                       SELECT user_id,
                              COUNT(*)          AS guesses_played,
                              AVG(distance_km)  AS avg_distance_km,
                              MIN(distance_km)  AS best_distance_km
                         FROM answers
                        WHERE distance_km IS NOT NULL
                        GROUP BY user_id
                   ) a ON a.user_id = rs.user_id
                  GROUP BY rs.user_id",
            )?;
            let rows = stmt.query_map([], |r| Ok(ScoreLeaderboardEntry {
                user_id:          r.get(0)?,
                total_score:      r.get(1)?,
                rounds_played:    r.get(2)?,
                guesses_played:   r.get(3)?,
                avg_distance_km:  r.get(4)?,
                best_distance_km: r.get(5)?,
            }))?;
            rows.map(|r| r.context("reading score leaderboard row")).collect()
        })
        .await
    }

    /// Rolling leaderboard — only rounds from the last 90 days.
    pub async fn score_leaderboard(&self) -> Result<Vec<ScoreLeaderboardEntry>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT rs.user_id,
                        SUM(rs.total_score)                AS total_score,
                        COUNT(DISTINCT rs.round_id)        AS rounds_played,
                        COALESCE(a.guesses_played, 0)      AS guesses_played,
                        COALESCE(a.avg_distance_km, 0.0)   AS avg_distance_km,
                        COALESCE(a.best_distance_km, 0.0)  AS best_distance_km
                   FROM round_scores rs
                   JOIN rounds ro ON ro.id = rs.round_id
                    AND ro.started_at >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-90 days')
                   LEFT JOIN (
                       SELECT ans.user_id,
                              COUNT(*)             AS guesses_played,
                              AVG(ans.distance_km) AS avg_distance_km,
                              MIN(ans.distance_km) AS best_distance_km
                         FROM answers ans
                         JOIN guesses g  ON g.id  = ans.guess_id
                         JOIN rounds  ri ON ri.id = g.round_id
                        WHERE ans.distance_km IS NOT NULL
                          AND ri.started_at >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-90 days')
                        GROUP BY ans.user_id
                   ) a ON a.user_id = rs.user_id
                  GROUP BY rs.user_id",
            )?;
            let rows = stmt.query_map([], |r| Ok(ScoreLeaderboardEntry {
                user_id:          r.get(0)?,
                total_score:      r.get(1)?,
                rounds_played:    r.get(2)?,
                guesses_played:   r.get(3)?,
                avg_distance_km:  r.get(4)?,
                best_distance_km: r.get(5)?,
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

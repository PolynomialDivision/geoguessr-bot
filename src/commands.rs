use anyhow::{anyhow, Result};
use chrono::{Datelike as _, TimeZone as _, Timelike as _};
use chrono_tz::Tz;
use matrix_sdk::ruma::OwnedUserId;
use tracing::error;

use crate::{
    config::ScheduleConfig,
    game::{self, format_dist},
    state::ScheduledOnce,
    BotContext,
};

pub async fn handle(ctx: &BotContext, sender: &OwnedUserId, body: &str) -> Result<Option<String>> {
    let cmd = body.split_whitespace().next().unwrap_or("").to_lowercase();

    match cmd.as_str() {
        "!startgeo" | "!geoguessr" => cmd_startgeo(ctx, sender).await,
        "!cancelgeo" => cmd_cancelgeo(ctx, sender, body).await,
        "!schedulegeo" => cmd_schedulegeo(ctx, sender, body).await,
        "!setschedule" => cmd_setschedule(ctx, sender, body).await,
        "!prefetch" => cmd_prefetch(ctx, sender).await,
        "!resetstats" => cmd_resetstats(ctx, sender, body).await,
        "!scores" | "!leaderboard" => cmd_scores(ctx).await,
        "!scores90" | "!leaderboard90" => cmd_scores_rolling(ctx).await,
        "!scoresmonth" | "!leaderboardmonth" | "!monthlyscores" => cmd_scores_month(ctx).await,
        "!mystats" => cmd_mystats(ctx, sender).await,
        "!countries" => cmd_countries(ctx).await,
        "!gameinfo" => cmd_gameinfo(ctx).await,
        "!fastest" => cmd_fastest(ctx).await,
        "!lang" => cmd_lang(ctx, sender, body).await,
        "!help" => Ok(Some(help_text())),
        _ => Ok(None),
    }
}

fn require_admin(ctx: &BotContext, sender: &OwnedUserId) -> Result<()> {
    if ctx.admin_users.contains(sender) {
        Ok(())
    } else {
        Err(anyhow!("__not_admin__"))
    }
}

// ── !startgeo ─────────────────────────────────────────────────────────────────

async fn cmd_startgeo(ctx: &BotContext, sender: &OwnedUserId) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    {
        let ag = ctx.active_game.lock().await;
        if ag.is_some() {
            return Ok(Some("⚠️ A game is already in progress!".to_owned()));
        }
    }

    let ctx2 = ctx.clone();
    let client = ctx.client.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = game::start_round(ctx2, client, true, None, None).await {
            error!("Manual game error: {e}");
        }
    });
    *ctx.round_abort.lock().await = Some(handle.abort_handle());

    Ok(Some(format!(
        "🌍 Starting! {} guess(es) · {} each.",
        ctx.effective_guesses_per_round().await,
        crate::game::format_duration(ctx.effective_answer_timeout().await),
    )))
}

// ── !cancelgeo ────────────────────────────────────────────────────────────────

async fn cmd_cancelgeo(
    ctx: &BotContext,
    sender: &OwnedUserId,
    body: &str,
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    let time_arg = body
        .splitn(2, char::is_whitespace)
        .nth(1)
        .unwrap_or("")
        .trim()
        .trim_matches(|c| c == '"' || c == '\'');

    // !cancelgeo HH:MM — cancel a pending one-time game.
    if !time_arg.is_empty() {
        let (qh, qm) = match ScheduleConfig::parse_game_time(time_arg) {
            Some(t) => t,
            None => {
                return Ok(Some(format!(
                    "❌ Invalid time \"{time_arg}\" · use HH:MM (e.g. 15:00)"
                )))
            }
        };
        let game_time = format!("{qh:02}:{qm:02}");
        let mut state = ctx.state.lock().await;
        let before = state.scheduled_once.len();
        state.scheduled_once.retain(|e| e.game_time != game_time);
        let removed = before - state.scheduled_once.len();
        state.save(&ctx.state_path).await?;
        return if removed == 0 {
            Ok(Some(format!("⚠️ No scheduled game found for {game_time}.")))
        } else {
            Ok(Some(format!(
                "✅ Cancelled {removed} scheduled game(s) at {game_time}."
            )))
        };
    }

    // !cancelgeo — abort the currently running round (join phase or active game).
    let abort = ctx.round_abort.lock().await.take();
    let had_game = ctx.active_game.lock().await.take().is_some();

    let round_id_to_close = {
        let mut st = ctx.state.lock().await;
        let round_id = st.active_round.as_ref().map(|ar| ar.round_id);
        st.active_round = None;
        st.pending_join = None;
        st.save(&ctx.state_path).await.ok();
        round_id
    };

    let had_join = round_id_to_close.is_some() || had_game;

    if let Some(round_id) = round_id_to_close {
        ctx.db.finish_round(round_id).await.ok();
    }

    // Clear join-phase state and web tokens.
    {
        let mut js = ctx.join_state.lock().await;
        js.message_event_id = None;
        js.participants.clear();
    }
    {
        let mut store = ctx.web_tokens.lock().await;
        store.tokens.clear();
        store.sessions.clear();
    }

    if let Some(handle) = abort {
        handle.abort();
    }

    if had_game || had_join {
        Ok(Some("🛑 Round cancelled.".to_owned()))
    } else {
        Ok(Some("⚠️ No active round to cancel.".to_owned()))
    }
}

// ── !schedulegeo ──────────────────────────────────────────────────────────────
//
// Usage:
//   !schedulegeo                                  — list pending one-time games
//   !schedulegeo HH:MM                            — schedule with config defaults
//   !schedulegeo HH:MM reminder=N                 — join-window before game (seconds)
//   !schedulegeo HH:MM timeout=N                  — answer timeout (seconds)
//   !schedulegeo HH:MM guesses=N                  — locations per round
//   !schedulegeo HH:MM reminder=N timeout=N guesses=N
//
// Schedules for today if the game time is still in the future; tomorrow otherwise.
// If the join window has already started but the game hasn't, the window is trimmed.

async fn cmd_schedulegeo(
    ctx: &BotContext,
    sender: &OwnedUserId,
    body: &str,
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    let args: Vec<&str> = body.split_whitespace().skip(1).collect();

    // No args → list pending.
    if args.is_empty() {
        let entries = ctx.state.lock().await.scheduled_once.clone();
        if entries.is_empty() {
            return Ok(Some(
                "No one-time games scheduled.\n\
                 Usage: !schedulegeo HH:MM [reminder=<secs>] [timeout=<secs>]"
                    .to_owned(),
            ));
        }
        let mut lines = vec!["📅 Pending one-time games:".to_owned()];
        for e in &entries {
            let reminder_str = e
                .reminder_before_secs
                .map(|s| format!(", reminder {}", crate::game::format_duration(s)))
                .unwrap_or_default();
            let timeout_str = e
                .answer_timeout_secs
                .map(|s| format!(", timeout {}", crate::game::format_duration(s)))
                .unwrap_or_default();
            let guesses_str = e
                .guesses_per_round
                .map(|n| format!(", {n} guess{}", if n == 1 { "" } else { "es" }))
                .unwrap_or_default();
            lines.push(format!(
                "  • {} on {}{reminder_str}{timeout_str}{guesses_str}",
                e.game_time, e.date
            ));
        }
        return Ok(Some(lines.join("\n")));
    }

    // First token is the time.
    let time_arg = args[0];
    let (qh, qm) = match ScheduleConfig::parse_game_time(time_arg) {
        Some(t) => t,
        None => {
            return Ok(Some(format!(
                "❌ Invalid time \"{time_arg}\" · use HH:MM (e.g. 15:00)"
            )))
        }
    };

    // Parse optional key=value overrides.
    let mut reminder_override: Option<u64> = None;
    let mut timeout_override: Option<u64> = None;
    let mut guesses_override: Option<u32> = None;
    for arg in &args[1..] {
        if let Some(v) = arg.strip_prefix("reminder=") {
            match v.parse::<u64>() {
                Ok(n) => reminder_override = Some(n),
                Err(_) => {
                    return Ok(Some(format!(
                        "❌ Invalid reminder \"{v}\" · must be seconds."
                    )))
                }
            }
        } else if let Some(v) = arg.strip_prefix("timeout=") {
            match v.parse::<u64>() {
                Ok(n) => timeout_override = Some(n),
                Err(_) => {
                    return Ok(Some(format!(
                        "❌ Invalid timeout \"{v}\" · must be seconds."
                    )))
                }
            }
        } else if let Some(v) = arg.strip_prefix("guesses=") {
            match v.parse::<u32>() {
                Ok(n) if n >= 1 => guesses_override = Some(n),
                Ok(_) => return Ok(Some("❌ guesses must be at least 1.".to_owned())),
                Err(_) => {
                    return Ok(Some(format!(
                        "❌ Invalid guesses \"{v}\" · must be a number."
                    )))
                }
            }
        } else {
            return Ok(Some(format!(
                "❌ Unknown argument \"{arg}\"\nUsage: !schedulegeo HH:MM [reminder=N] [timeout=N] [guesses=N]"
            )));
        }
    }

    let reminder = reminder_override.unwrap_or(ctx.config.schedule.reminder_before_secs);

    let tz: Tz = ctx
        .config
        .schedule
        .timezone
        .parse()
        .unwrap_or(chrono_tz::UTC);
    let local_now = chrono::Utc::now().with_timezone(&tz);

    let game_secs = (qh * 3600 + qm * 60) as i64;
    let fire_secs = (game_secs - reminder as i64).rem_euclid(86400);
    let now_secs = (local_now.hour() * 3600 + local_now.minute() * 60 + local_now.second()) as i64;

    // Reject if the reminder is longer than the time from midnight to the game —
    // that wraps the fire time to later in the day than the game itself.
    if fire_secs > game_secs {
        let hrs = reminder / 3600;
        let mins = (reminder % 3600) / 60;
        let mut msg = format!(
            "❌ reminder={reminder}s (~{hrs}h {mins}m) is longer than the time from midnight to {qh:02}:{qm:02} \
             — the join window would open at {:02}:{:02}, after the game itself.",
            (fire_secs / 3600) as u32,
            ((fire_secs % 3600) / 60) as u32,
        );
        let as_secs = reminder / 1000;
        if as_secs >= 1 && as_secs as i64 <= game_secs {
            msg.push_str(&format!(" (Did you mean reminder={as_secs}?)"));
        }
        return Ok(Some(msg));
    }

    // If the game itself has already passed today → schedule for tomorrow.
    // If only the join window has passed but the game is still future → schedule
    // for today with a truncated reminder (fire as soon as possible).
    let (date, effective_reminder_override, truncated) = if now_secs >= game_secs {
        (
            local_now.date_naive() + chrono::Duration::days(1),
            reminder_override,
            false,
        )
    } else if now_secs >= fire_secs {
        // Join window started already; trim reminder so fire_time ≥ now + 60s.
        let secs_left = (game_secs - now_secs).max(60) as u64;
        let trimmed = secs_left.saturating_sub(60); // fire ~1 min from now
        (local_now.date_naive(), Some(trimmed), trimmed < reminder)
    } else {
        (local_now.date_naive(), reminder_override, false)
    };

    let effective_reminder =
        effective_reminder_override.unwrap_or(ctx.config.schedule.reminder_before_secs);
    let eff_fire_secs = (game_secs - effective_reminder as i64).rem_euclid(86400);
    let fire_hour = (eff_fire_secs / 3600) as u32;
    let fire_min = ((eff_fire_secs % 3600) / 60) as u32;

    let game_time = format!("{qh:02}:{qm:02}");
    let entry = ScheduledOnce {
        game_time: game_time.clone(),
        date,
        reminder_before_secs: effective_reminder_override,
        answer_timeout_secs: timeout_override,
        guesses_per_round: guesses_override,
    };

    {
        let mut state = ctx.state.lock().await;
        if state
            .scheduled_once
            .iter()
            .any(|e| e.game_time == game_time && e.date == date)
        {
            return Ok(Some(format!(
                "⚠️ A game at {game_time} on {date} is already scheduled."
            )));
        }
        state.scheduled_once.push(entry);
        state.save(&ctx.state_path).await?;
    }

    let day_str = if date == local_now.date_naive() {
        "today".to_owned()
    } else {
        "tomorrow".to_owned()
    };

    let mut detail = format!("✅ GeoGuessr: {day_str} at {game_time}");
    if effective_reminder > 0 {
        detail.push_str(&format!(" (join ~{fire_hour:02}:{fire_min:02})"));
    }
    if truncated {
        detail.push_str(&format!(
            " ⚠️ join window trimmed to {} (was {})",
            crate::game::format_duration(effective_reminder),
            crate::game::format_duration(reminder),
        ));
    }
    if let Some(t) = timeout_override {
        detail.push_str(&format!(", timeout {}", crate::game::format_duration(t)));
    }
    if let Some(g) = guesses_override {
        detail.push_str(&format!(", {g} guess{}", if g == 1 { "" } else { "es" }));
    }
    detail.push_str(&format!("\nCancel: !cancelgeo {game_time}"));
    Ok(Some(detail))
}

// ── !prefetch ─────────────────────────────────────────────────────────────────

async fn cmd_prefetch(ctx: &BotContext, sender: &OwnedUserId) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    let before = ctx.state.lock().await.cached_guesses.len();
    game::prefetch_if_needed(ctx, before + 5).await;
    let after = ctx.state.lock().await.cached_guesses.len();
    Ok(Some(format!("✅ Image cache: {before} → {after}")))
}

// ── !setschedule ──────────────────────────────────────────────────────────────
//
// Usage:
//   !setschedule                          — show current runtime overrides
//   !setschedule guesses=N               — override guesses per round
//   !setschedule photos=N                — override photos per guess location
//   !setschedule timeout=N               — override answer timeout (seconds)
//   !setschedule reset                   — clear all runtime overrides

async fn cmd_setschedule(
    ctx: &BotContext,
    sender: &OwnedUserId,
    body: &str,
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    let args: Vec<&str> = body.split_whitespace().skip(1).collect();

    if args.is_empty() {
        let ov = ctx.state.lock().await.schedule_overrides.clone();
        let guesses = ov
            .guesses_per_round
            .map(|n| n.to_string())
            .unwrap_or_else(|| format!("{} (config)", ctx.config.schedule.guesses_per_round));
        let photos = ov
            .photos_per_location
            .map(|n| n.to_string())
            .unwrap_or_else(|| format!("{} (config)", ctx.config.schedule.photos_per_location));
        let timeout = ov
            .answer_timeout_secs
            .map(|s| crate::game::format_duration(s))
            .unwrap_or_else(|| {
                format!(
                    "{} (config)",
                    crate::game::format_duration(ctx.config.schedule.answer_timeout_secs)
                )
            });
        return Ok(Some(format!(
            "📅 Schedule overrides:\n· guesses: {guesses}\n· photos/guess: {photos}\n· timeout: {timeout}\nChange: !setschedule [guesses=N] [photos=N] [timeout=N] | !setschedule reset"
        )));
    }

    if args == ["reset"] {
        {
            let mut st = ctx.state.lock().await;
            st.schedule_overrides = Default::default();
            st.save(&ctx.state_path).await?;
        }
        return Ok(Some(
            "✅ Schedule overrides cleared — using config defaults.".to_owned(),
        ));
    }

    let mut guesses_override: Option<u32> = None;
    let mut photos_override: Option<usize> = None;
    let mut timeout_override: Option<u64> = None;
    for arg in &args {
        if let Some(v) = arg.strip_prefix("guesses=") {
            match v.parse::<u32>() {
                Ok(n) if n >= 1 => guesses_override = Some(n),
                Ok(_) => return Ok(Some("❌ guesses must be at least 1.".to_owned())),
                Err(_) => {
                    return Ok(Some(format!(
                        "❌ Invalid guesses \"{v}\" · must be a number."
                    )))
                }
            }
        } else if let Some(v) = arg.strip_prefix("photos=") {
            match v.parse::<usize>() {
                Ok(n) if n >= 1 => photos_override = Some(n),
                Ok(_) => return Ok(Some("❌ photos must be at least 1.".to_owned())),
                Err(_) => {
                    return Ok(Some(format!(
                        "❌ Invalid photos \"{v}\" · must be a number."
                    )))
                }
            }
        } else if let Some(v) = arg.strip_prefix("timeout=") {
            match v.parse::<u64>() {
                Ok(n) => timeout_override = Some(n),
                Err(_) => {
                    return Ok(Some(format!(
                        "❌ Invalid timeout \"{v}\" · must be seconds."
                    )))
                }
            }
        } else {
            return Ok(Some(format!(
                "❌ Unknown argument \"{arg}\"\nUsage: !setschedule [guesses=N] [photos=N] [timeout=N] | reset"
            )));
        }
    }

    {
        let mut st = ctx.state.lock().await;
        if let Some(n) = guesses_override {
            st.schedule_overrides.guesses_per_round = Some(n);
        }
        if let Some(n) = photos_override {
            st.schedule_overrides.photos_per_location = Some(n);
        }
        if let Some(n) = timeout_override {
            st.schedule_overrides.answer_timeout_secs = Some(n);
        }
        st.save(&ctx.state_path).await?;
    }

    let mut parts = Vec::new();
    if let Some(n) = guesses_override {
        parts.push(format!("{n} guess{}", if n == 1 { "" } else { "es" }));
    }
    if let Some(n) = photos_override {
        parts.push(format!("{n} photo{}/guess", if n == 1 { "" } else { "s" }));
    }
    if let Some(n) = timeout_override {
        parts.push(format!("timeout {}", crate::game::format_duration(n)));
    }
    Ok(Some(format!("✅ Schedule updated: {}", parts.join(", "))))
}

// ── !resetstats ───────────────────────────────────────────────────────────────

async fn cmd_resetstats(
    ctx: &BotContext,
    sender: &OwnedUserId,
    body: &str,
) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    let confirmed = body.split_whitespace().nth(1).unwrap_or("") == "confirm";
    if !confirmed {
        return Ok(Some(
            "⚠️ This deletes ALL game history.\nConfirm: !resetstats confirm".to_owned(),
        ));
    }

    match ctx.db.reset_stats().await {
        Ok(()) => Ok(Some("✅ All stats have been reset.".to_owned())),
        Err(e) => {
            error!("reset_stats failed: {e}");
            Ok(Some("❌ Reset failed — check the logs.".to_owned()))
        }
    }
}

// ── !scores / !leaderboard ────────────────────────────────────────────────────

async fn cmd_scores(ctx: &BotContext) -> Result<Option<String>> {
    cmd_scores_free_guess(ctx).await
}

async fn cmd_scores_free_guess(ctx: &BotContext) -> Result<Option<String>> {
    match build_alltime_leaderboard(ctx).await {
        Some(text) => Ok(Some(text)),
        None => Ok(Some(
            "No scores yet — no games have been played.".to_owned(),
        )),
    }
}

async fn cmd_scores_rolling(ctx: &BotContext) -> Result<Option<String>> {
    match build_rolling_leaderboard(ctx).await {
        Some(text) => Ok(Some(text)),
        None => Ok(Some("No scores in the last 90 days.".to_owned())),
    }
}

async fn cmd_scores_month(ctx: &BotContext) -> Result<Option<String>> {
    match build_monthly_leaderboard(ctx).await {
        Some(text) => Ok(Some(text)),
        None => Ok(Some("No scores yet this month.".to_owned())),
    }
}

// ── Scoring / rating ─────────────────────────────────────────────────────────
//
// The leaderboard rating is a Bayesian-shrinkage estimate of a player's true
// skill:
//
//   rating = (n / (n + k)) * player_average + (k / (n + k)) * baseline
//
// `n` is `guesses_played` — every guess a player was on the hook for, real
// submissions AND missed/no-guess rounds (recorded as 0, see
// `Db::record_missed_guesses`). Counting misses as 0 rather than excluding
// them is what makes "every round you join counts": skipping a hard guess
// drags your average down instead of just not counting, so there's no
// strategic benefit to only answering the easy ones.
//
// `baseline` is the community's own average score per guess — derived from
// the leaderboard data itself (see `community_baseline`), not a fixed
// constant, so it tracks the actual difficulty/skill level of this room's
// games over time.
//
// `k` (config: `schedule.rating.k`) controls how fast a player's rating
// trusts their own average over the baseline: at n = k they're weighted
// equally; at n >> k the rating is essentially their raw average; at n = 0
// the rating is exactly the baseline. This is what keeps a single lucky
// perfect round from outranking someone with a long, consistently good
// track record, while a player who plays a lot but badly still converges to
// their own (low) rating rather than being propped up by participation
// alone — shrinkage only pulls toward the baseline, it never adds points
// for volume.

fn raw_score_average(entry: &crate::db::ScoreLeaderboardEntry) -> f64 {
    if entry.guesses_played > 0 {
        entry.total_score as f64 / entry.guesses_played as f64
    } else {
        0.0
    }
}

/// Community baseline: average score per guess across the whole board
/// (real + missed guesses). Falls back to `fallback` only when there's no
/// data yet to derive one from (e.g. right after `!resetstats`).
fn community_baseline(board: &[crate::db::ScoreLeaderboardEntry], fallback: f64) -> f64 {
    let total_score: i64 = board.iter().map(|e| e.total_score).sum();
    let total_guesses: i64 = board.iter().map(|e| e.guesses_played).sum();
    if total_guesses > 0 {
        total_score as f64 / total_guesses as f64
    } else {
        fallback
    }
}

/// Bayesian-shrinkage rating — see the module-level comment above.
fn shrunk_rating(entry: &crate::db::ScoreLeaderboardEntry, k: f64, baseline: f64) -> f64 {
    let n = entry.guesses_played as f64;
    if n <= 0.0 {
        return baseline;
    }
    let weight = n / (n + k.max(0.0));
    weight * raw_score_average(entry) + (1.0 - weight) * baseline
}

/// Percentage of `guesses_played` that were real submissions (not missed).
fn completion_pct(entry: &crate::db::ScoreLeaderboardEntry) -> f64 {
    if entry.guesses_played > 0 {
        entry.guesses_answered as f64 / entry.guesses_played as f64 * 100.0
    } else {
        0.0
    }
}

/// A rating is "provisional" — not yet enough data to trust — below the
/// configured `provisional_threshold` of guesses played. Display-only; the
/// rating math already accounts for sample size via shrinkage regardless.
fn is_provisional(entry: &crate::db::ScoreLeaderboardEntry, threshold: i64) -> bool {
    entry.guesses_played < threshold
}

fn sort_score_leaderboard(board: &mut [crate::db::ScoreLeaderboardEntry], k: f64, baseline: f64) {
    board.sort_by(|a, b| {
        shrunk_rating(b, k, baseline)
            .partial_cmp(&shrunk_rating(a, k, baseline))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.guesses_played.cmp(&a.guesses_played))
            .then(a.user_id.cmp(&b.user_id))
    });
}

fn format_leaderboard(
    mut board: Vec<crate::db::ScoreLeaderboardEntry>,
    header: &str,
    round_count: i64,
    rating_cfg: &crate::config::RatingConfig,
) -> String {
    const BAR_W: usize = 10;

    let baseline = community_baseline(&board, rating_cfg.baseline_fallback);
    let k = rating_cfg.k;
    sort_score_leaderboard(&mut board, k, baseline);

    let mut lines = vec![
        format!("🏆 **{}** · {} rounds", header, round_count),
        format!(
            "⭐ rating blends your average with the community baseline (⭐{}) until you've played \
             enough to trust it · 🔰 = provisional (<{} guesses)",
            baseline.round() as i64,
            rating_cfg.provisional_threshold,
        ),
        String::new(),
    ];
    for (i, entry) in board.iter().enumerate() {
        let medal = match i {
            0 => "🥇",
            1 => "🥈",
            2 => "🥉",
            _ => "▪️",
        };
        let rating = shrunk_rating(entry, k, baseline);
        let raw_avg = raw_score_average(entry);
        let pct = completion_pct(entry);
        let best_dist = if entry.guesses_answered > 0 {
            format_dist(entry.best_distance_km)
        } else {
            "n/a".to_owned()
        };
        let filled = ((rating / 5000.0) * BAR_W as f64).round() as usize;
        let bar = format!(
            "{}{}",
            "█".repeat(filled.min(BAR_W)),
            "░".repeat(BAR_W - filled.min(BAR_W))
        );
        let provisional = if is_provisional(entry, rating_cfg.provisional_threshold) {
            " 🔰"
        } else {
            ""
        };
        lines.push(format!(
            "{medal} {}. {}{provisional} · ⭐{} {bar} · 🎯{}/g · ✅{}% · 🎮{} · 🏅{}",
            i + 1,
            entry.user_id,
            rating.round() as i64,
            raw_avg.round() as i64,
            pct.round() as i64,
            entry.guesses_played,
            best_dist,
        ));
    }
    lines.join("\n")
}

/// All-time leaderboard (every round ever played). Used by `!leaderboard`.
pub async fn build_alltime_leaderboard(ctx: &BotContext) -> Option<String> {
    let board = ctx.db.score_leaderboard_alltime().await.ok()?;
    if board.is_empty() {
        return None;
    }
    let round_count = ctx.db.round_count().await.unwrap_or(0);
    Some(format_leaderboard(
        board,
        "All-time Leaderboard",
        round_count,
        &ctx.config.schedule.rating,
    ))
}

/// 90-day rolling leaderboard.
pub async fn build_rolling_leaderboard(ctx: &BotContext) -> Option<String> {
    let board = ctx.db.score_leaderboard().await.ok()?;
    if board.is_empty() {
        return None;
    }
    let round_count = ctx.db.round_count().await.unwrap_or(0);
    Some(format_leaderboard(
        board,
        "Leaderboard (last 90 days)",
        round_count,
        &ctx.config.schedule.rating,
    ))
}

/// Start of the current calendar month, in `tz`, as a UTC instant — the SQL
/// lower bound for "this month's" rounds. Uses the bot's configured
/// timezone (`schedule.timezone`, UTC if unset/invalid) so the monthly
/// leaderboard rolls over at local, not UTC, midnight on the 1st. On the
/// rare local-midnight DST gap, falls back to the current wall-clock
/// instant, which is still safely within the target month.
fn current_month_start_utc(tz: Tz) -> chrono::DateTime<chrono::Utc> {
    month_start_utc(tz, chrono::Utc::now().with_timezone(&tz))
}

/// Start of the calendar month containing `local_now` (already expressed in
/// `tz`), as a UTC instant. Split out from `current_month_start_utc` so the
/// month/year-rollover math can be unit-tested without depending on the
/// real wall clock.
fn month_start_utc(tz: Tz, local_now: chrono::DateTime<Tz>) -> chrono::DateTime<chrono::Utc> {
    tz.with_ymd_and_hms(local_now.year(), local_now.month(), 1, 0, 0, 0)
        .earliest()
        .unwrap_or(local_now)
        .with_timezone(&chrono::Utc)
}

/// Calendar-month leaderboard — only rounds from the current month (in the
/// bot's configured timezone). Posted after each round instead of the
/// all-time leaderboard so recent activity stays prominent; `!scores` /
/// `!leaderboard` remain the all-time view, and `!scoresmonth` /
/// `!leaderboardmonth` request this one explicitly. Uses the exact same
/// rating math as the all-time leaderboard (`format_leaderboard`) — only
/// which rounds are included differs, computed straight from persisted
/// round timestamps, so there's no separate monthly state to keep in sync
/// and a restart or month rollover can't duplicate or drift scores.
pub async fn build_monthly_leaderboard(ctx: &BotContext) -> Option<String> {
    let tz: Tz = ctx.config.schedule.timezone.parse().unwrap_or(chrono_tz::UTC);
    let month_start = current_month_start_utc(tz);

    let board = ctx.db.score_leaderboard_month(month_start).await.ok()?;
    if board.is_empty() {
        return None;
    }
    let round_count = ctx.db.round_count_since(month_start).await.unwrap_or(0);
    let header = format!("{} Leaderboard", chrono::Utc::now().with_timezone(&tz).format("%B %Y"));
    Some(format_leaderboard(
        board,
        &header,
        round_count,
        &ctx.config.schedule.rating,
    ))
}

// ── !mystats ──────────────────────────────────────────────────────────────────

async fn cmd_mystats(ctx: &BotContext, sender: &OwnedUserId) -> Result<Option<String>> {
    let user = sender.as_str();

    let stats = match ctx.db.user_stats(user).await {
        Err(e) => {
            error!("DB user_stats error: {e}");
            return Ok(Some("❌ Could not read stats from database.".to_owned()));
        }
        Ok(None) => return Ok(Some("No rounds played yet.".to_owned())),
        Ok(Some(s)) => s,
    };

    let pct = if stats.total_questions > 0 {
        stats.total_correct * 100 / stats.total_questions
    } else {
        0
    };

    let rating_cfg = &ctx.config.schedule.rating;
    let mut board = ctx.db.score_leaderboard_alltime().await.unwrap_or_default();
    let baseline = community_baseline(&board, rating_cfg.baseline_fallback);
    sort_score_leaderboard(&mut board, rating_cfg.k, baseline);
    let rank = board.iter().position(|e| e.user_id == user).map(|i| i + 1);
    let rank_str = rank
        .map(|r| format!(" · rank #{r} of {}", board.len()))
        .unwrap_or_default();

    let mut lines = vec![format!(
        "🌍 **Your stats** · {}/{} correct ({}%) · {} round(s){rank_str}",
        stats.total_correct, stats.total_questions, pct, stats.rounds_played,
    )];

    if let Some(entry) = board.iter().find(|e| e.user_id == user) {
        let rating = shrunk_rating(entry, rating_cfg.k, baseline);
        let raw_avg = raw_score_average(entry);
        let pct = completion_pct(entry);
        let provisional = if is_provisional(entry, rating_cfg.provisional_threshold) {
            " (provisional)"
        } else {
            ""
        };
        lines.push(format!(
            "⭐ Rating {}{provisional} · avg {} pts/guess · {}% completion ({} guesses, {} missed)",
            rating.round() as i64,
            raw_avg.round() as i64,
            pct.round() as i64,
            entry.guesses_played,
            entry.guesses_played - entry.guesses_answered,
        ));
    }

    let country_stats = ctx.db.user_country_stats(user).await.unwrap_or_default();
    if country_stats.len() >= 2 {
        let best = country_stats.first().unwrap();
        let worst = country_stats.last().unwrap();
        let best_pct = best.correct * 100 / best.answered;
        let worst_pct = worst.correct * 100 / worst.answered;
        lines.push(format!("🏆 Best: {} ({}%)", best.country, best_pct));
        lines.push(format!("😬 Worst: {} ({}%)", worst.country, worst_pct));
    }

    Ok(Some(lines.join("\n")))
}

// ── !countries ────────────────────────────────────────────────────────────────

async fn cmd_countries(ctx: &BotContext) -> Result<Option<String>> {
    let stats = match ctx.db.country_stats().await {
        Ok(s) => s,
        Err(e) => {
            error!("country_stats: {e}");
            return Ok(Some(
                "❌ Could not read country stats from database.".to_owned(),
            ));
        }
    };
    if stats.is_empty() {
        return Ok(Some("No guesses yet.".to_owned()));
    }

    let total_q: i64 = stats.iter().map(|s| s.times_asked).sum();
    let max_asked: i64 = stats.iter().map(|s| s.times_asked).max().unwrap_or(1);

    let mut lines = vec![
        format!("🗺️ **Countries** · {} guesses", total_q),
        String::new(),
    ];

    const BAR_W: usize = 10;
    for s in &stats {
        let filled = (s.times_asked * BAR_W as i64 / max_asked) as usize;
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(BAR_W - filled));
        let pct = if s.total_answers > 0 {
            s.correct_answers * 100 / s.total_answers
        } else {
            0
        };
        lines.push(format!(
            "{bar}  {:>2}x  {:>3}% ✓  {} ({})",
            s.times_asked, pct, s.country, s.region,
        ));
    }

    Ok(Some(lines.join("\n")))
}

// ── !fastest ──────────────────────────────────────────────────────────────────

async fn cmd_fastest(ctx: &BotContext) -> Result<Option<String>> {
    let board = match ctx.db.speed_leaderboard().await {
        Ok(b) => b,
        Err(e) => {
            error!("speed_leaderboard: {e}");
            return Ok(Some(
                "❌ Could not read speed stats from database.".to_owned(),
            ));
        }
    };
    if board.is_empty() {
        return Ok(Some(
            "Not enough data yet · need at least 3 correct answers per player.".to_owned(),
        ));
    }

    let mut lines = vec![
        "⚡ **Speed** · fastest correct answers (min. 3 samples)".to_owned(),
        String::new(),
    ];
    for (i, e) in board.iter().enumerate() {
        let medal = match i {
            0 => "🥇",
            1 => "🥈",
            2 => "🥉",
            _ => "  ",
        };
        lines.push(format!(
            "{medal} {:>2}. {} : {:.1}s avg · {} correct",
            i + 1,
            e.user_id,
            e.avg_secs,
            e.sample_count,
        ));
    }

    Ok(Some(lines.join("\n")))
}

// ── !gameinfo ─────────────────────────────────────────────────────────────────

async fn cmd_gameinfo(ctx: &BotContext) -> Result<Option<String>> {
    let s = &ctx.config.schedule;
    let tz = &s.timezone;

    let times_str = if s.game_times.is_empty() {
        "not scheduled".to_owned()
    } else {
        s.game_times.join(", ")
    };

    let reminder_line = if s.reminder_before_secs > 0 {
        format!(
            "\n⏰ Join window {} early · react with {} to play",
            game::format_duration(s.reminder_before_secs),
            s.join_emoji,
        )
    } else {
        String::new()
    };

    let mode_line = "🗺️ Free guess · type a location in private chat\n\
                     City, country, full address, or lat,lon · scored by distance";

    let photos_line = if s.photos_per_location > 1 {
        format!("\n📸 {} photos per guess", s.photos_per_location)
    } else {
        String::new()
    };

    let msg = format!(
        "🌍 **GeoGuessr**\n\
         🕐 {times_str} ({tz}){reminder_line}\n\
         📍 {} guess{} · {}{photos_line}\n\n\
         {mode_line}",
        s.guesses_per_round,
        if s.guesses_per_round == 1 { "" } else { "es" },
        game::format_duration(s.answer_timeout_secs),
    );

    Ok(Some(msg))
}

// ── !lang ─────────────────────────────────────────────────────────────────────

async fn cmd_lang(ctx: &BotContext, sender: &OwnedUserId, body: &str) -> Result<Option<String>> {
    let arg = body.split_whitespace().nth(1).unwrap_or("").trim();

    if arg.is_empty() {
        let current = ctx
            .state
            .lock()
            .await
            .user_langs
            .get(sender.as_str())
            .cloned()
            .unwrap_or_else(|| "en".to_owned());
        let label = game::lang_label(&current);
        return Ok(Some(format!(
            "Your language: **{current}** ({label})\n\
             Change with: !lang <flag>  or  !lang <code>  (e.g. !lang 🇩🇪  or  !lang de)"
        )));
    }

    // Try as a flag emoji, then as a BCP-47 code.
    let lang = if let Some(l) = game::flag_to_lang(arg) {
        l
    } else if let Some(l) = game::text_code_to_lang(&arg.to_lowercase()) {
        l
    } else {
        return Ok(Some(format!(
            "Unknown language «{arg}»\n\
             Use !lang <flag>  or  !lang <code>  (e.g. !lang 🇩🇪  or  !lang de)"
        )));
    };

    let label = game::lang_label(lang);

    {
        let mut st = ctx.state.lock().await;
        st.user_langs
            .insert(sender.as_str().to_owned(), lang.to_owned());
        st.save(&ctx.state_path).await?;
    }

    Ok(Some(format!(
        "Language set to **{lang}** ({label}) · your guess locations will now appear in {label}"
    )))
}

// ── !help ─────────────────────────────────────────────────────────────────────

fn help_text() -> String {
    "🌍 **GeoGuessr** commands:

  !gameinfo          · schedule & how to play
  !scores            · all-time leaderboard
  !scoresmonth       · this month's leaderboard
  !scores90          · last 90 days leaderboard
  !mystats           · your rank and stats
  !countries         · country accuracy chart
  !fastest           · speed leaderboard
  !lang <flag or code>   · set your language (e.g. !lang 🇩🇪  or  !lang de)
  !help              · show this help

**Admin:**
  !startgeo                                  · start a round immediately (no join phase)
  !cancelgeo                                 · abort current round
  !cancelgeo HH:MM                           · cancel a scheduled game
  !schedulegeo HH:MM [reminder=N] [timeout=N] [guesses=N]
                                             · schedule a game (today if possible)
  !setschedule [guesses=N] [photos=N] [timeout=N]  · override daily schedule defaults
  !setschedule reset                         · clear overrides, use config values
  !prefetch                                  · fill the image cache
  !resetstats confirm                        · wipe all history"
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RatingConfig;
    use crate::db::ScoreLeaderboardEntry;

    /// A player with no missed guesses: `guesses_answered == guesses_played`,
    /// matching every pre-existing (pre-`missed`-column) row.
    fn score_entry(user_id: &str, total_score: i64, guesses_played: i64) -> ScoreLeaderboardEntry {
        score_entry_with_misses(user_id, total_score, guesses_played, guesses_played)
    }

    /// A player with `guesses_played - guesses_answered` missed guesses
    /// (each contributing 0 to `total_score`, matching how
    /// `Db::record_missed_guesses` stores them).
    fn score_entry_with_misses(
        user_id: &str,
        total_score: i64,
        guesses_played: i64,
        guesses_answered: i64,
    ) -> ScoreLeaderboardEntry {
        ScoreLeaderboardEntry {
            user_id: user_id.to_owned(),
            total_score,
            rounds_played: guesses_played,
            guesses_played,
            guesses_answered,
            avg_distance_km: 1000.0,
            best_distance_km: 100.0,
        }
    }

    const TEST_RATING: RatingConfig = RatingConfig {
        k: 15.0,
        provisional_threshold: 10,
        baseline_fallback: 1500.0,
    };

    // ── Missed guesses count as zero ────────────────────────────────────────

    #[test]
    fn missed_guess_drags_average_down_instead_of_being_ignored() {
        // One perfect 5000-pt guess, then one missed guess (0 pts). If the
        // miss were simply excluded (the old bug), this would still average
        // 5000/1 = 5000. Counted as a zero, it should average 5000/2 = 2500.
        let entry = score_entry_with_misses("@skipper:example.com", 5_000, 2, 1);

        assert_eq!(raw_score_average(&entry), 2500.0);
        assert_eq!(completion_pct(&entry), 50.0);
    }

    #[test]
    fn fully_missed_player_has_zero_average_not_no_average() {
        let entry = score_entry_with_misses("@ghost:example.com", 0, 3, 0);

        assert_eq!(raw_score_average(&entry), 0.0);
        assert_eq!(completion_pct(&entry), 0.0);
    }

    // ── One lucky round shouldn't dominate ──────────────────────────────────

    #[test]
    fn one_perfect_round_does_not_outrank_sustained_consistent_play() {
        // A single perfect round (raw average 5000, n=1) vs. 50 rounds of
        // solid (not perfect) play averaging 2200.
        let lucky = score_entry("@lucky-one-shot:example.com", 5_000, 1);
        let consistent = score_entry("@consistent-grinder:example.com", 2_200 * 50, 50);

        let baseline = 1800.0;
        let rating_lucky = shrunk_rating(&lucky, TEST_RATING.k, baseline);
        let rating_consistent = shrunk_rating(&consistent, TEST_RATING.k, baseline);

        assert!(
            rating_consistent > rating_lucky,
            "consistent play ({rating_consistent}) should outrank a single lucky round ({rating_lucky})"
        );

        let mut board = vec![lucky, consistent];
        sort_score_leaderboard(&mut board, TEST_RATING.k, baseline);
        assert_eq!(board[0].user_id, "@consistent-grinder:example.com");
    }

    // ── Many consistently good rounds → rating trusts the player's own average ──

    #[test]
    fn many_good_rounds_converge_close_to_raw_average() {
        let veteran = score_entry("@veteran:example.com", 2_500 * 200, 200);
        let baseline = 1800.0;

        let rating = shrunk_rating(&veteran, TEST_RATING.k, baseline);
        let raw_avg = raw_score_average(&veteran);

        // n=200 vastly outweighs k=15, so the baseline's pull should be minor.
        assert!(
            (rating - raw_avg).abs() < 100.0,
            "rating {rating} should be close to raw average {raw_avg} at large n"
        );
    }

    // ── Grinding badly isn't rewarded merely for participation ─────────────

    #[test]
    fn repeated_bad_play_is_not_propped_up_by_participation() {
        // Plays constantly but badly: raw average 200, far below baseline.
        let bad_grinder = score_entry("@bad-grinder:example.com", 200 * 100, 100);
        // Barely plays, but happens to land right at the baseline average.
        let mediocre_newcomer = score_entry("@mediocre-newcomer:example.com", 1_800 * 2, 2);

        let baseline = 1800.0;
        let rating_bad = shrunk_rating(&bad_grinder, TEST_RATING.k, baseline);
        let rating_newcomer = shrunk_rating(&mediocre_newcomer, TEST_RATING.k, baseline);

        // Volume alone must not lift the bad grinder above someone who merely
        // played a couple of average rounds — the rating should track their
        // own poor average, not their participation count.
        assert!(
            rating_bad < rating_newcomer,
            "grinding badly ({rating_bad}) must not outrank a mediocre newcomer ({rating_newcomer})"
        );
        // And it should sit close to their own (bad) average, not be pulled
        // meaningfully toward the baseline just because n is large.
        assert!(
            rating_bad < baseline - 1000.0,
            "rating {rating_bad} should stay well below the baseline {baseline}"
        );
    }

    // ── Baseline is a genuine (weighted) community average ─────────────────

    #[test]
    fn community_baseline_is_guess_weighted_not_a_naive_average_of_averages() {
        let a = score_entry("@a:example.com", 1_000, 10); // avg 100
        let b = score_entry("@b:example.com", 100_000, 100); // avg 1000

        // Weighted: (1000 + 100000) / (10 + 100) = 918.18...
        // A naive mean of the two per-player averages would give 550 instead
        // — wrong, because it would let a low-volume player skew the
        // baseline as much as a high-volume one.
        let baseline = community_baseline(&[a, b], 1500.0);
        assert!((baseline - 918.18).abs() < 0.1, "baseline was {baseline}");
    }

    #[test]
    fn community_baseline_falls_back_when_no_data() {
        assert_eq!(community_baseline(&[], 1500.0), 1500.0);
    }

    // ── Existing (pre-`missed`-column) data keeps working ───────────────────

    #[test]
    fn legacy_style_entries_with_no_misses_behave_sanely() {
        // Every historical row defaults to missed=0, so guesses_answered ==
        // guesses_played for all pre-existing data — completion should read
        // 100% and the rating/average math should behave exactly as before.
        let legacy = score_entry("@veteran-since-day-one:example.com", 4_000 * 30, 30);

        assert_eq!(completion_pct(&legacy), 100.0);
        assert_eq!(raw_score_average(&legacy), 4_000.0);
        assert!(!is_provisional(&legacy, TEST_RATING.provisional_threshold));
    }

    // ── Sorting + formatting ─────────────────────────────────────────────────

    #[test]
    fn sort_score_leaderboard_orders_by_rating_then_volume_then_name() {
        let mut board = vec![
            score_entry("@high-total-low-average:example.com", 100_000, 100),
            score_entry("@lower-total-high-average:example.com", 50_000, 10),
        ];

        sort_score_leaderboard(&mut board, TEST_RATING.k, 1800.0);

        assert_eq!(board[0].user_id, "@lower-total-high-average:example.com");
    }

    #[test]
    fn provisional_flag_reflects_threshold() {
        let new_player = score_entry("@new:example.com", 5_000, 3);
        let established = score_entry("@established:example.com", 100_000, 30);

        assert!(is_provisional(&new_player, TEST_RATING.provisional_threshold));
        assert!(!is_provisional(&established, TEST_RATING.provisional_threshold));
    }

    #[test]
    fn leaderboard_formatting_shows_rating_average_completion_and_volume() {
        // Single entry → the community baseline equals its own average, so
        // shrinkage is a no-op and the rating is exactly the raw average —
        // keeps the expected string deterministic.
        let text = format_leaderboard(
            vec![score_entry("@twenty-guesses:example.com", 100_000, 20)],
            "Test Leaderboard",
            1,
            &TEST_RATING,
        );

        assert!(text.contains(
            "🥇 1. @twenty-guesses:example.com · ⭐5000 ██████████ · 🎯5000/g · ✅100% · 🎮20 · 🏅100 km"
        ));
    }

    #[test]
    fn leaderboard_formatting_marks_provisional_entries() {
        let text = format_leaderboard(
            vec![score_entry("@fresh:example.com", 5_000, 2)],
            "Test Leaderboard",
            1,
            &TEST_RATING,
        );

        assert!(text.contains("@fresh:example.com 🔰"));
    }

    // ── Monthly leaderboard ──────────────────────────────────────────────────

    #[test]
    fn month_start_utc_handles_december_to_january_rollover() {
        let tz = chrono_tz::UTC;
        let local_now = tz.with_ymd_and_hms(2026, 1, 5, 10, 0, 0).unwrap();

        let start = month_start_utc(tz, local_now);

        assert_eq!(start.to_rfc3339(), "2026-01-01T00:00:00+00:00");
    }

    #[test]
    fn month_start_utc_uses_configured_timezone_not_bare_utc() {
        let tz: Tz = "Europe/Berlin".parse().unwrap();
        // Jan 1st 00:30 in Berlin (UTC+1 in winter) is still Dec 31st in UTC —
        // the month boundary must follow the *local* midnight, not UTC's.
        let local_now = tz.with_ymd_and_hms(2026, 1, 1, 0, 30, 0).unwrap();

        let start = month_start_utc(tz, local_now);

        assert_eq!(start.to_rfc3339(), "2025-12-31T23:00:00+00:00");
    }

    /// Monthly and all-time leaderboards share the exact same sort/format
    /// pipeline (`format_leaderboard` → `sort_score_leaderboard`) — the only
    /// difference between them is which rounds the DB query includes before
    /// handing the board over. So tied entries (equal rating, equal volume)
    /// must break the tie identically in both: alphabetically by user_id.
    #[test]
    fn ties_break_identically_regardless_of_leaderboard_type() {
        let build_tied_board = || {
            vec![
                score_entry("@zed:example.com", 50_000, 10),
                score_entry("@amy:example.com", 50_000, 10),
            ]
        };

        let mut alltime_shaped = build_tied_board();
        let mut month_shaped = build_tied_board();
        sort_score_leaderboard(&mut alltime_shaped, TEST_RATING.k, 1800.0);
        sort_score_leaderboard(&mut month_shaped, TEST_RATING.k, 1800.0);

        assert_eq!(alltime_shaped[0].user_id, "@amy:example.com");
        assert_eq!(
            alltime_shaped.iter().map(|e| &e.user_id).collect::<Vec<_>>(),
            month_shaped.iter().map(|e| &e.user_id).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn monthly_leaderboard_uses_a_month_year_header_and_mention_ready_ids() {
        let text = format_leaderboard(
            vec![score_entry("@alice:example.com", 100_000, 20)],
            "August 2026 Leaderboard",
            5,
            &TEST_RATING,
        );

        assert!(text.starts_with("🏆 **August 2026 Leaderboard** · 5 rounds"));
        // Raw "@user:server" mxid, unmodified — `format::mentionify` (applied
        // uniformly wherever command output and round summaries are sent,
        // see main.rs/game.rs) turns this into a proper mention pill. The
        // formatter itself must never pre-render a display name or it would
        // bypass that pipeline.
        assert!(text.contains("@alice:example.com"));
    }
}

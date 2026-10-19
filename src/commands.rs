use anyhow::Result;
use chrono::{Datelike as _, TimeZone as _, Timelike as _};
use chrono_tz::Tz;
use matrix_sdk::ruma::OwnedUserId;
use mxbot_common::matrix_sdk;
use tracing::error;

use crate::{
    config::ScheduleConfig,
    duels::{self, Standing},
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
    Ok(mxbot_common::admin::require_admin(
        &ctx.admin_users,
        sender,
    )?)
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
        .split_once(char::is_whitespace)
        .map(|x| x.1)
        .unwrap_or("")
        .trim()
        .trim_matches(|c| c == '"' || c == '\'');

    // !cancelgeo HH:MM: cancel a pending one-time game.
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

    // !cancelgeo: abort the currently running round (join phase or active game).
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
//   !schedulegeo: list pending one-time games
//   !schedulegeo HH:MM: schedule with config defaults
//   !schedulegeo HH:MM reminder=N: join-window before game (seconds)
//   !schedulegeo HH:MM timeout=N: answer timeout (seconds)
//   !schedulegeo HH:MM guesses=N: locations per round
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
                "❌ Unknown argument \"{arg}\"\n💡 Usage: !schedulegeo HH:MM [reminder=N] [timeout=N] [guesses=N]"
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

    // Reject if the reminder is longer than the time from midnight to the game.
    // That wraps the fire time to later in the day than the game itself.
    if fire_secs > game_secs {
        let hrs = reminder / 3600;
        let mins = (reminder % 3600) / 60;
        let mut msg = format!(
            "❌ reminder={reminder}s (~{hrs}h {mins}m) is longer than the time from midnight to {qh:02}:{qm:02}.\n\
             ⏰ The join window would open at {:02}:{:02}, after the game itself.",
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
    detail.push_str(&format!("\n🚫 Cancel: !cancelgeo {game_time}"));
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
//   !setschedule: show current runtime overrides
//   !setschedule guesses=N: override guesses per round
//   !setschedule photos=N: override photos per guess location
//   !setschedule timeout=N: override answer timeout (seconds)
//   !setschedule reset: clear all runtime overrides

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
            .map(crate::game::format_duration)
            .unwrap_or_else(|| {
                format!(
                    "{} (config)",
                    crate::game::format_duration(ctx.config.schedule.answer_timeout_secs)
                )
            });
        return Ok(Some(format!(
            "📅 Schedule overrides\n🎯 Guesses: {guesses}\n📸 Photos per guess: {photos}\n⏳ Timeout: {timeout}\n\n✏️ !setschedule [guesses=N] [photos=N] [timeout=N]\n↩️ !setschedule reset"
        )));
    }

    if args == ["reset"] {
        {
            let mut st = ctx.state.lock().await;
            st.schedule_overrides = Default::default();
            st.save(&ctx.state_path).await?;
        }
        return Ok(Some(
            "✅ Schedule overrides cleared. Using the config defaults.".to_owned(),
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
                "❌ Unknown argument \"{arg}\"\n💡 Usage: !setschedule [guesses=N] [photos=N] [timeout=N] or reset"
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
            Ok(Some("❌ Reset failed. Check the logs.".to_owned()))
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
            "🤷 No scores yet. No games have been played.".to_owned(),
        )),
    }
}

async fn cmd_scores_rolling(ctx: &BotContext) -> Result<Option<String>> {
    match build_rolling_leaderboard(ctx).await {
        Some(text) => Ok(Some(text)),
        None => Ok(Some("🤷 No scores in the last 90 days.".to_owned())),
    }
}

async fn cmd_scores_month(ctx: &BotContext) -> Result<Option<String>> {
    match build_monthly_leaderboard(ctx).await {
        Some(text) => Ok(Some(text)),
        None => Ok(Some("🤷 No scores yet this month.".to_owned())),
    }
}

// ── Leaderboards ─────────────────────────────────────────────────────────────
//
// Every board is built from duels (see `duels`) straight from the stored
// distances, so an image's result never shifts after it closed. The month
// is a season: duel points add up, so playing more counts. The 90-day and
// all-time boards measure skill: the share of duels won, ranked once a
// player has `leaderboard.min_images` images.

/// How a board is ranked.
#[derive(Clone, Copy)]
enum Order {
    /// Most duel points (the month).
    Points,
    /// Best share of duels won, from this many images (90 days, all time).
    Rate { min_images: i64 },
}

/// "⚔️ 20 of 25 duels · 80%", or a note when there was nobody to duel.
fn duel_line(s: &Standing) -> String {
    match s.rate() {
        Some(rate) => format!(
            "⚔️ {} of {} duels · {}%",
            duels::fmt_points(s.halves),
            s.duels,
            (rate * 100.0).round() as i64
        ),
        None => "⚔️ no duels yet".to_owned(),
    }
}

/// "👑 11 · 🎮 17 · 😴 2 · 🏅 3 m": misses and the closest guess only when
/// there are any.
fn record_line(s: &Standing) -> String {
    let mut parts = vec![format!("👑 {}", s.wins), format!("🎮 {}", s.images)];
    if s.missed > 0 {
        parts.push(format!("😴 {}", s.missed));
    }
    if let Some(best) = s.best_km {
        parts.push(format!("🏅 {}", format_dist(best)));
    }
    parts.join(" · ")
}

fn format_leaderboard(
    mut board: Vec<Standing>,
    header: &str,
    round_count: i64,
    order: Order,
) -> String {
    let mut lines = vec![format!("🏆 **{header}** · {round_count} rounds")];
    let (ranked, unranked) = match order {
        Order::Points => {
            lines.push("⚔️ +1 for every player you beat on an image, ½ for a tie.".to_owned());
            duels::sort_by_points(&mut board);
            (board, Vec::new())
        }
        Order::Rate { min_images } => {
            lines.push(format!(
                "⚔️ Ranked by the share of duels won, from {min_images} images."
            ));
            let (mut ranked, mut unranked): (Vec<_>, Vec<_>) = board
                .into_iter()
                .partition(|s| s.images >= min_images && s.duels > 0);
            duels::sort_by_rate(&mut ranked);
            unranked.sort_by(|a, b| b.images.cmp(&a.images).then(a.user_id.cmp(&b.user_id)));
            (ranked, unranked)
        }
    };
    lines.push("😴 Joined but no guess loses the duel.".to_owned());
    lines.push("👑 images won · 🎮 played · 🏅 closest".to_owned());

    for (i, s) in ranked.iter().enumerate() {
        let medal = match i {
            0 => "🥇",
            1 => "🥈",
            2 => "🥉",
            _ => "▪️",
        };
        lines.push(String::new());
        lines.push(format!("{medal} {}. {}", i + 1, s.user_id));
        lines.push(duel_line(s));
        lines.push(record_line(s));
    }
    if let (Order::Rate { min_images }, false) = (order, unranked.is_empty()) {
        lines.push(String::new());
        lines.push(format!("🔰 Not ranked yet, under {min_images} images:"));
        for s in &unranked {
            lines.push(format!("▪️ {} · 🎮 {}", s.user_id, s.images));
        }
    }
    lines.join("\n")
}

fn min_images(ctx: &BotContext) -> i64 {
    ctx.config.schedule.leaderboard.min_images.max(1)
}

/// All-time leaderboard (every round ever played). Used by `!leaderboard`.
pub async fn build_alltime_leaderboard(ctx: &BotContext) -> Option<String> {
    let board = duels::standings(&ctx.db.duel_entries(None).await.ok()?);
    if board.is_empty() {
        return None;
    }
    let round_count = ctx.db.round_count().await.unwrap_or(0);
    Some(format_leaderboard(
        board,
        "All-time Leaderboard",
        round_count,
        Order::Rate {
            min_images: min_images(ctx),
        },
    ))
}

/// 90-day rolling leaderboard.
pub async fn build_rolling_leaderboard(ctx: &BotContext) -> Option<String> {
    let since = chrono::Utc::now() - chrono::Duration::days(90);
    let board = duels::standings(&ctx.db.duel_entries(Some(since)).await.ok()?);
    if board.is_empty() {
        return None;
    }
    let round_count = ctx.db.round_count_since(since).await.unwrap_or(0);
    Some(format_leaderboard(
        board,
        "Leaderboard (last 90 days)",
        round_count,
        Order::Rate {
            min_images: min_images(ctx),
        },
    ))
}

/// Start of the current calendar month, in `tz`, as a UTC instant. The SQL
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

/// Calendar-month leaderboard, only rounds from the current month (in the
/// bot's configured timezone). Posted after each round instead of the
/// all-time leaderboard so recent activity stays prominent; `!scores` /
/// `!leaderboard` remain the all-time view, and `!scoresmonth` /
/// `!leaderboardmonth` request this one explicitly. Ranked by duel points,
/// computed straight from persisted round timestamps, so there's no
/// separate monthly state to keep in sync and a restart or month rollover
/// can't duplicate or drift scores.
pub async fn build_monthly_leaderboard(ctx: &BotContext) -> Option<String> {
    let tz: Tz = ctx
        .config
        .schedule
        .timezone
        .parse()
        .unwrap_or(chrono_tz::UTC);
    let month_start = current_month_start_utc(tz);

    let board = duels::standings(&ctx.db.duel_entries(Some(month_start)).await.ok()?);
    if board.is_empty() {
        return None;
    }
    let round_count = ctx.db.round_count_since(month_start).await.unwrap_or(0);
    let header = format!(
        "{} Leaderboard",
        chrono::Utc::now().with_timezone(&tz).format("%B %Y")
    );
    Some(format_leaderboard(
        board,
        &header,
        round_count,
        Order::Points,
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
        Ok(None) => return Ok(Some("🤷 No rounds played yet.".to_owned())),
        Ok(Some(s)) => s,
    };

    let pct = if stats.total_questions > 0 {
        stats.total_correct * 100 / stats.total_questions
    } else {
        0
    };

    let min_images = min_images(ctx);
    let mut board = duels::standings(&ctx.db.duel_entries(None).await.unwrap_or_default());
    board.retain(|s| s.user_id == user || (s.images >= min_images && s.duels > 0));
    duels::sort_by_rate(&mut board);
    let me = board.iter().find(|s| s.user_id == user);
    let rank_str = match me {
        Some(s) if s.images >= min_images && s.duels > 0 => {
            let rank = board.iter().position(|e| e.user_id == user).unwrap_or(0) + 1;
            let ranked = board
                .iter()
                .filter(|e| e.images >= min_images && e.duels > 0)
                .count();
            format!(" · rank #{rank} of {ranked}")
        }
        _ => String::new(),
    };

    let mut lines = vec![format!(
        "🌍 **Your stats** · {}/{} correct ({}%) · {} round(s){rank_str}",
        stats.total_correct, stats.total_questions, pct, stats.rounds_played,
    )];

    if let Some(s) = me {
        lines.push(duel_line(s));
        lines.push(record_line(s));
        if s.images < min_images {
            lines.push(format!("🔰 Ranked from {min_images} images."));
        }
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
        return Ok(Some("🤷 No guesses yet.".to_owned()));
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
            "{bar} {}x · ✅ {}% · {} ({})",
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
            "⏳ Not enough data yet.\nEach player needs 3 correct answers.".to_owned(),
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
            _ => "▪️",
        };
        lines.push(format!(
            "{medal} {}. {} · {:.1}s avg · {} correct",
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

    let mode_line = "🗺️ Free guess: type a place in your private chat.\n\
                     🏙️ City, country, address or lat,lon.\n\
                     ⚔️ Closest wins: +1 for every player you beat, ½ for a tie.\n\
                     😴 Joined but no guess loses the duel.";

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
            "🌐 Your language: **{current}** ({label})\n\
             ✏️ Change it with !lang <flag> or !lang <code>, e.g. !lang 🇩🇪 or !lang de"
        )));
    }

    // Try as a flag emoji, then as a BCP-47 code.
    let lang = if let Some(l) = game::flag_to_lang(arg) {
        l
    } else if let Some(l) = game::text_code_to_lang(&arg.to_lowercase()) {
        l
    } else {
        return Ok(Some(format!(
            "🤔 Unknown language «{arg}»\n\
             💡 Use !lang <flag> or !lang <code>, e.g. !lang 🇩🇪 or !lang de"
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
        "✅ Language set to **{lang}** ({label}).\n🗺️ Your guess locations now appear in {label}."
    )))
}

// ── !help ─────────────────────────────────────────────────────────────────────

fn help_text() -> String {
    "🌍 **GeoGuessr**

ℹ️ !gameinfo · schedule and how to play
🏆 !scores · all-time ranking
📅 !scoresmonth · this month
🗓️ !scores90 · last 90 days
📊 !mystats · your rank and stats
🗺️ !countries · country accuracy
⚡ !fastest · speed ranking
🌐 !lang <flag or code> · your language, e.g. !lang 🇩🇪
❔ !help · this help

🛠️ **Admin**
▶️ !startgeo · start now (no join phase)
🛑 !cancelgeo · abort the current round
🚫 !cancelgeo HH:MM · cancel a scheduled game
⏰ !schedulegeo HH:MM [reminder=N] [timeout=N] [guesses=N]
⚙️ !setschedule [guesses=N] [photos=N] [timeout=N]
↩️ !setschedule reset · back to config values
📥 !prefetch · fill the image cache
🧹 !resetstats confirm · wipe all history"
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standing(user: &str, halves: i64, duels: i64, images: i64) -> Standing {
        Standing {
            user_id: user.to_owned(),
            halves,
            duels,
            wins: halves / 4,
            images,
            missed: 0,
            best_km: Some(0.003),
        }
    }

    // ── Leaderboard text ─────────────────────────────────────────────────────

    #[test]
    fn the_month_ranks_by_duel_points_and_explains_them() {
        let text = format_leaderboard(
            vec![
                standing("@sharp:example.com", 20, 10, 5),
                standing("@busy:example.com", 41, 40, 17),
            ],
            "October 2026 Leaderboard",
            17,
            Order::Points,
        );
        assert!(text.starts_with("🏆 **October 2026 Leaderboard** · 17 rounds"));
        assert!(text.contains("+1 for every player you beat"));
        let busy = text
            .find("🥇 1. @busy:example.com")
            .expect("busy leads on points");
        assert!(text.find("🥈 2. @sharp:example.com").unwrap() > busy);
        assert!(text.contains("⚔️ 20½ of 40 duels · 51%"));
        assert!(text.contains("👑 10 · 🎮 17 · 🏅 3 m"));
        assert!(!text.contains("pts"), "no more 5000-point scale");
        assert!(!text.contains(" | "));
    }

    #[test]
    fn the_long_run_ranks_by_share_and_lists_newcomers_apart() {
        let text = format_leaderboard(
            vec![
                standing("@sharp:example.com", 40, 25, 20),
                standing("@busy:example.com", 60, 60, 40),
                standing("@lucky:example.com", 4, 2, 2),
            ],
            "All-time Leaderboard",
            40,
            Order::Rate { min_images: 20 },
        );
        let sharp = text
            .find("🥇 1. @sharp:example.com")
            .expect("80% beats 50%");
        assert!(text.find("🥈 2. @busy:example.com").unwrap() > sharp);
        assert!(text.contains(
            "🔰 Not ranked yet, under 20 images:
▪️ @lucky:example.com · 🎮 2"
        ));
        assert!(
            !text.contains(". @lucky"),
            "two lucky images don't top the board"
        );
    }

    #[test]
    fn misses_and_solo_players_read_plainly() {
        let mut s = standing("@solo:example.com", 0, 0, 3);
        s.missed = 1;
        s.best_km = None;
        assert_eq!(duel_line(&s), "⚔️ no duels yet");
        assert_eq!(record_line(&s), "👑 0 · 🎮 3 · 😴 1");
    }

    #[test]
    fn mention_ready_ids_are_left_for_mentionify() {
        let text = format_leaderboard(
            vec![standing("@alice:example.com", 2, 1, 1)],
            "August 2026 Leaderboard",
            5,
            Order::Points,
        );
        // Raw "@user:server" mxid, unmodified: `format::mentionify` (applied
        // uniformly wherever command output and round summaries are sent,
        // see main.rs/game.rs) turns this into a proper mention pill.
        assert!(text.contains("@alice:example.com"));
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
        // Jan 1st 00:30 in Berlin (UTC+1 in winter) is still Dec 31st in UTC.
        // The month boundary must follow the *local* midnight, not UTC's.
        let local_now = tz.with_ymd_and_hms(2026, 1, 1, 0, 30, 0).unwrap();

        let start = month_start_utc(tz, local_now);

        assert_eq!(start.to_rfc3339(), "2025-12-31T23:00:00+00:00");
    }
}

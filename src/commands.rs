use anyhow::{anyhow, Result};
use chrono::Timelike as _;
use chrono_tz::Tz;
use matrix_sdk::ruma::OwnedUserId;
use tracing::error;

use crate::{BotContext, config::ScheduleConfig, game::{self, format_dist}, state::ScheduledOnce};

pub async fn handle(ctx: &BotContext, sender: &OwnedUserId, body: &str) -> Result<Option<String>> {
    let cmd = body.split_whitespace().next().unwrap_or("").to_lowercase();

    match cmd.as_str() {
        "!startgeo"
        | "!geoguessr"   => cmd_startgeo(ctx, sender).await,
        "!cancelgeo"     => cmd_cancelgeo(ctx, sender, body).await,
        "!schedulegeo"   => cmd_schedulegeo(ctx, sender, body).await,
        "!prefetch"      => cmd_prefetch(ctx, sender).await,
        "!resetstats"    => cmd_resetstats(ctx, sender, body).await,
        "!scores"
        | "!leaderboard" => cmd_scores(ctx).await,
        "!mystats"       => cmd_mystats(ctx, sender).await,
        "!countries"     => cmd_countries(ctx).await,
        "!gameinfo"      => cmd_gameinfo(ctx).await,
        "!fastest"       => cmd_fastest(ctx).await,
        "!help"          => Ok(Some(help_text())),
        _                => Ok(None),
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

    let ctx2   = ctx.clone();
    let client = ctx.client.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = game::start_round(ctx2, client, true, None, None).await {
            error!("Manual game error: {e}");
        }
    });
    *ctx.round_abort.lock().await = Some(handle.abort_handle());

    Ok(Some(format!(
        "🌍 GeoGuessr starting! {} images, {} per image.",
        ctx.config.schedule.images_per_round,
        crate::game::format_duration(ctx.config.schedule.answer_timeout_secs),
    )))
}

// ── !cancelgeo ────────────────────────────────────────────────────────────────

async fn cmd_cancelgeo(ctx: &BotContext, sender: &OwnedUserId, body: &str) -> Result<Option<String>> {
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
            None    => return Ok(Some(format!(
                "❌ Invalid time \"{time_arg}\" — use HH:MM, e.g. !cancelgeo 15:00"
            ))),
        };
        let game_time = format!("{qh:02}:{qm:02}");
        let mut state  = ctx.state.lock().await;
        let before     = state.scheduled_once.len();
        state.scheduled_once.retain(|e| e.game_time != game_time);
        let removed    = before - state.scheduled_once.len();
        state.save(&ctx.state_path).await?;
        return if removed == 0 {
            Ok(Some(format!("⚠️ No scheduled game found for {game_time}.")))
        } else {
            Ok(Some(format!("✅ Cancelled {removed} scheduled game(s) at {game_time}.")))
        };
    }

    // !cancelgeo — abort the currently running round (join phase or active game).
    let abort = ctx.round_abort.lock().await.take();
    let had_game = ctx.active_game.lock().await.take().is_some();
    let had_join = {
        let mut st = ctx.state.lock().await;
        let had = st.pending_join.is_some();
        st.pending_join = None;
        st.save(&ctx.state_path).await.ok();
        had
    };
    // Clear any DM-room mappings too.
    ctx.dm_rooms.lock().await.clear();
    {
        let mut js = ctx.join_state.lock().await;
        js.message_event_id = None;
        js.participants.clear();
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
//   !schedulegeo                         — list pending one-time games
//   !schedulegeo HH:MM                   — schedule with config defaults
//   !schedulegeo HH:MM reminder=N        — override reminder window (seconds)
//   !schedulegeo HH:MM timeout=N         — override answer timeout (seconds)
//   !schedulegeo HH:MM reminder=N timeout=N

async fn cmd_schedulegeo(ctx: &BotContext, sender: &OwnedUserId, body: &str) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    let args: Vec<&str> = body.split_whitespace().skip(1).collect();

    // No args → list pending.
    if args.is_empty() {
        let entries = ctx.state.lock().await.scheduled_once.clone();
        if entries.is_empty() {
            return Ok(Some(
                "No one-time games scheduled.\n\
                 Usage: !schedulegeo HH:MM [reminder=<secs>] [timeout=<secs>]".to_owned()
            ));
        }
        let mut lines = vec!["📅 Pending one-time games:".to_owned()];
        for e in &entries {
            let reminder_str = e.reminder_before_secs
                .map(|s| format!(", reminder {}",  crate::game::format_duration(s)))
                .unwrap_or_default();
            let timeout_str = e.answer_timeout_secs
                .map(|s| format!(", timeout {}", crate::game::format_duration(s)))
                .unwrap_or_default();
            lines.push(format!("  • {} on {}{reminder_str}{timeout_str}", e.game_time, e.date));
        }
        return Ok(Some(lines.join("\n")));
    }

    // First token is the time.
    let time_arg = args[0];
    let (qh, qm) = match ScheduleConfig::parse_game_time(time_arg) {
        Some(t) => t,
        None    => return Ok(Some(format!(
            "❌ Invalid time \"{time_arg}\" — use HH:MM, e.g. !schedulegeo 15:00"
        ))),
    };

    // Parse optional key=value overrides.
    let mut reminder_override: Option<u64> = None;
    let mut timeout_override:  Option<u64> = None;
    for arg in &args[1..] {
        if let Some(v) = arg.strip_prefix("reminder=") {
            match v.parse::<u64>() {
                Ok(n)  => reminder_override = Some(n),
                Err(_) => return Ok(Some(format!("❌ Invalid reminder value \"{v}\" — must be seconds."))),
            }
        } else if let Some(v) = arg.strip_prefix("timeout=") {
            match v.parse::<u64>() {
                Ok(n)  => timeout_override = Some(n),
                Err(_) => return Ok(Some(format!("❌ Invalid timeout value \"{v}\" — must be seconds."))),
            }
        } else {
            return Ok(Some(format!(
                "❌ Unknown argument \"{arg}\".\n\
                 Usage: !schedulegeo HH:MM [reminder=<secs>] [timeout=<secs>]"
            )));
        }
    }

    let reminder = reminder_override
        .unwrap_or(ctx.config.schedule.reminder_before_secs);

    let tz: Tz    = ctx.config.schedule.timezone.parse().unwrap_or(chrono_tz::UTC);
    let local_now = chrono::Utc::now().with_timezone(&tz);

    let game_secs = (qh * 3600 + qm * 60) as i64;
    let fire_secs = (game_secs - reminder as i64).rem_euclid(86400);
    let now_secs  = (local_now.hour() * 3600
        + local_now.minute() * 60
        + local_now.second()) as i64;

    // If the fire moment has already passed today, schedule for tomorrow.
    let date = if now_secs >= fire_secs {
        local_now.date_naive() + chrono::Duration::days(1)
    } else {
        local_now.date_naive()
    };

    let game_time = format!("{qh:02}:{qm:02}");
    let entry = ScheduledOnce {
        game_time:           game_time.clone(),
        date,
        reminder_before_secs: reminder_override,
        answer_timeout_secs:  timeout_override,
    };

    {
        let mut state = ctx.state.lock().await;
        if state.scheduled_once.iter().any(|e| e.game_time == game_time && e.date == date) {
            return Ok(Some(format!(
                "⚠️ A game at {game_time} on {date} is already scheduled."
            )));
        }
        state.scheduled_once.push(entry);
        state.save(&ctx.state_path).await?;
    }

    let day_str   = if date == local_now.date_naive() { "today".to_owned() } else { "tomorrow".to_owned() };
    let fire_hour = (fire_secs / 3600) as u32;
    let fire_min  = ((fire_secs % 3600) / 60) as u32;

    let mut detail = format!("✅ GeoGuessr scheduled for {day_str} at {game_time}");
    if reminder > 0 {
        detail.push_str(&format!(
            " (join prompt at {fire_hour:02}:{fire_min:02})"
        ));
    }
    if let Some(t) = timeout_override {
        detail.push_str(&format!(", timeout {}", crate::game::format_duration(t)));
    }
    detail.push_str(&format!(".\nCancel with: !cancelgeo {game_time}"));
    Ok(Some(detail))
}

// ── !prefetch ─────────────────────────────────────────────────────────────────

async fn cmd_prefetch(ctx: &BotContext, sender: &OwnedUserId) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    let before = ctx.state.lock().await.cached_images.len();
    game::prefetch_if_needed(ctx, before + 5).await;
    let after = ctx.state.lock().await.cached_images.len();
    Ok(Some(format!("✅ Image cache: {before} → {after}")))
}

// ── !resetstats ───────────────────────────────────────────────────────────────

async fn cmd_resetstats(ctx: &BotContext, sender: &OwnedUserId, body: &str) -> Result<Option<String>> {
    require_admin(ctx, sender)?;

    let confirmed = body.split_whitespace().nth(1).unwrap_or("") == "confirm";
    if !confirmed {
        return Ok(Some(
            "⚠️ This will delete ALL game history — rounds, images, answers and scores.\n\
             To confirm: !resetstats confirm".to_owned()
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
    if ctx.config.schedule.game_mode == crate::config::GameMode::FreeGuess {
        return cmd_scores_free_guess(ctx).await;
    }

    // Multiple-choice mode — correct/total ranking.
    let board = match ctx.db.leaderboard().await {
        Ok(b)  => b,
        Err(e) => {
            error!("DB leaderboard error: {e}");
            return Ok(Some("❌ Could not read leaderboard from database.".to_owned()));
        }
    };
    if board.is_empty() {
        return Ok(Some("No scores yet — no games have been played.".to_owned()));
    }

    let round_count = ctx.db.round_count().await.unwrap_or(0);
    let mut lines = vec![format!("🏆 Leaderboard  ({} round(s) played)", round_count)];
    lines.push(String::new());
    for (i, entry) in board.iter().enumerate() {
        let pct   = if entry.total_questions > 0 {
            entry.total_correct * 100 / entry.total_questions
        } else { 0 };
        let medal = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "  " };
        lines.push(format!(
            "{medal} {:>2}. {} — {}/{} correct ({}%)",
            i + 1, entry.user_id, entry.total_correct, entry.total_questions, pct,
        ));
    }
    Ok(Some(lines.join("\n")))
}

async fn cmd_scores_free_guess(ctx: &BotContext) -> Result<Option<String>> {
    match build_alltime_leaderboard(ctx).await {
        Some(text) => Ok(Some(text)),
        None       => Ok(Some("No scores yet — no games have been played.".to_owned())),
    }
}

/// Build the all-time leaderboard text (Bayesian-ranked).
/// Returns `None` if the DB is empty.
pub async fn build_alltime_leaderboard(ctx: &BotContext) -> Option<String> {
    let mut board = ctx.db.score_leaderboard().await.ok()?;
    if board.is_empty() { return None; }

    const C: f64    = 10.0;
    const BAR_W: usize = 10;

    let total_imgs: i64  = board.iter().map(|e| e.images_played).sum();
    let total_pts:  i64  = board.iter().map(|e| e.total_score).sum();
    let global_mean: f64 = if total_imgs > 0 {
        total_pts as f64 / total_imgs as f64
    } else { 2000.0 };

    let bayesian = |e: &crate::db::ScoreLeaderboardEntry| -> f64 {
        let n   = e.images_played as f64;
        let avg = if n > 0.0 { e.total_score as f64 / n } else { 0.0 };
        (C * global_mean + n * avg) / (C + n)
    };

    board.sort_by(|a, b| bayesian(b).partial_cmp(&bayesian(a)).unwrap_or(std::cmp::Ordering::Equal));

    let round_count = ctx.db.round_count().await.unwrap_or(0);
    let mut lines = vec![
        format!("🏆 **All-time Leaderboard**  ({} round(s) · ranked by skill)", round_count),
        String::new(),
    ];

    for (i, entry) in board.iter().enumerate() {
        let medal = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "  " };
        let name  = entry.user_id
            .split(':').next().unwrap_or(&entry.user_id)
            .trim_start_matches('@');

        let b_avg  = bayesian(entry);
        let filled = ((b_avg / 5000.0) * BAR_W as f64).round() as usize;
        let bar    = format!("{}{}", "█".repeat(filled.min(BAR_W)), "░".repeat(BAR_W - filled.min(BAR_W)));
        let avg_dist  = if entry.images_played > 0 { format_dist(entry.avg_distance_km)  } else { "—".to_owned() };
        let best_dist = if entry.images_played > 0 { format_dist(entry.best_distance_km) } else { "—".to_owned() };

        lines.push(format!("{medal} {:>2}. {}  —  {} pts/img", i + 1, name, b_avg.round() as i64));
        lines.push(format!("      {bar}  ⌀ {}  🏅 {}  ({} imgs)", avg_dist, best_dist, entry.images_played));
    }
    Some(lines.join("\n"))
}

// ── !mystats ──────────────────────────────────────────────────────────────────

async fn cmd_mystats(ctx: &BotContext, sender: &OwnedUserId) -> Result<Option<String>> {
    let user = sender.as_str();

    let stats = match ctx.db.user_stats(user).await {
        Err(e) => {
            error!("DB user_stats error: {e}");
            return Ok(Some("❌ Could not read stats from database.".to_owned()));
        }
        Ok(None) => return Ok(Some("You haven't played any GeoGuessr rounds yet.".to_owned())),
        Ok(Some(s)) => s,
    };

    let pct = if stats.total_questions > 0 {
        stats.total_correct * 100 / stats.total_questions
    } else { 0 };

    let board = ctx.db.leaderboard().await.unwrap_or_default();
    let rank  = board.iter().position(|e| e.user_id == user).map(|i| i + 1);
    let rank_str = rank
        .map(|r| format!("  |  rank #{r} of {}", board.len()))
        .unwrap_or_default();

    let mut lines = vec![format!(
        "🌍 Your stats: {}/{} correct ({}%)  |  {} round(s) played{rank_str}",
        stats.total_correct, stats.total_questions, pct, stats.rounds_played,
    )];

    let country_stats = ctx.db.user_country_stats(user).await.unwrap_or_default();
    if country_stats.len() >= 2 {
        let best  = country_stats.first().unwrap();
        let worst = country_stats.last().unwrap();
        let best_pct  = best.correct  * 100 / best.answered;
        let worst_pct = worst.correct * 100 / worst.answered;
        lines.push(format!("🏆 Best country:  {} ({}%)", best.country,  best_pct));
        lines.push(format!("😬 Worst country: {} ({}%)", worst.country, worst_pct));
    }

    Ok(Some(lines.join("\n")))
}

// ── !countries ────────────────────────────────────────────────────────────────

async fn cmd_countries(ctx: &BotContext) -> Result<Option<String>> {
    let stats = match ctx.db.country_stats().await {
        Ok(s)  => s,
        Err(e) => {
            error!("country_stats: {e}");
            return Ok(Some("❌ Could not read country stats from database.".to_owned()));
        }
    };
    if stats.is_empty() {
        return Ok(Some("No images shown yet.".to_owned()));
    }

    let total_q: i64     = stats.iter().map(|s| s.times_asked).sum();
    let max_asked: i64   = stats.iter().map(|s| s.times_asked).max().unwrap_or(1);

    let mut lines = vec![
        format!("🗺️ Countries  ({} images shown)", total_q),
        String::new(),
    ];

    const BAR_W: usize = 10;
    for s in &stats {
        let filled = (s.times_asked * BAR_W as i64 / max_asked) as usize;
        let bar    = format!("{}{}", "█".repeat(filled), "░".repeat(BAR_W - filled));
        let pct    = if s.total_answers > 0 {
            s.correct_answers * 100 / s.total_answers
        } else { 0 };
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
        Ok(b)  => b,
        Err(e) => {
            error!("speed_leaderboard: {e}");
            return Ok(Some("❌ Could not read speed stats from database.".to_owned()));
        }
    };
    if board.is_empty() {
        return Ok(Some(
            "Not enough data yet — need at least 3 correct answers per player.".to_owned()
        ));
    }

    let mut lines = vec![
        "⚡ Speed Leaderboard  (correct answers only, min. 3 samples)".to_owned(),
        String::new(),
    ];
    for (i, e) in board.iter().enumerate() {
        let medal = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "  " };
        lines.push(format!(
            "{medal} {:>2}. {} — avg {:.1}s  ({} correct answers)",
            i + 1, e.user_id, e.avg_secs, e.sample_count,
        ));
    }

    Ok(Some(lines.join("\n")))
}

// ── !gameinfo ─────────────────────────────────────────────────────────────────

async fn cmd_gameinfo(ctx: &BotContext) -> Result<Option<String>> {
    let s  = &ctx.config.schedule;
    let tz = &s.timezone;

    let times_str = if s.game_times.is_empty() {
        "not scheduled".to_owned()
    } else {
        s.game_times.join(", ")
    };

    let reminder_line = if s.reminder_before_secs > 0 {
        format!(
            "\n  ⏰ **Join window** opens {} before game time — react with {} to opt in.",
            game::format_duration(s.reminder_before_secs),
            s.join_emoji,
        )
    } else {
        String::new()
    };

    let mode_line = match s.game_mode {
        crate::config::GameMode::MultipleChoice =>
            "  🗺️ **Mode** multiple choice — pick the right country from 4 options.\n\
             \n\
             **How to answer:** react with 🇦 🇧 🇨 🇩 or type **!a** / **!b** / **!c** / **!d**.".to_owned(),
        crate::config::GameMode::FreeGuess =>
            "  🗺️ **Mode** free guess — type any location in the private chat.\n\
             \n\
             **How to answer:** reply in the private chat with a city, country, full address, \
             or lat,lon coordinates. Scored by distance (5 000 pts at 0 km).".to_owned(),
    };

    let photos_line = if s.photos_per_location > 1 {
        format!("\n  📸 **{} photos** per location.", s.photos_per_location)
    } else {
        String::new()
    };

    let msg = format!(
        "🌍 **GeoGuessr Bot — game info**\n\
         \n\
           🕐 **Daily game** at {times_str} ({tz}).{reminder_line}\n\
         \n\
           🖼️ **{images}** image{plural} per round, **{timeout}** to answer each one.{photos_line}\n\
         \n\
         {mode_line}",
        images  = s.images_per_round,
        plural  = if s.images_per_round == 1 { "" } else { "s" },
        timeout = game::format_duration(s.answer_timeout_secs),
    );

    Ok(Some(msg))
}

// ── !help ─────────────────────────────────────────────────────────────────────

fn help_text() -> String {
    "🌍 GeoGuessr Bot commands:

  !gameinfo                — show schedule, mode and how to play
  !scores / !leaderboard   — overall ranking of all players
  !mystats                 — your personal score, rank and best/worst country
  !countries               — bar chart of every country shown with accuracy
  !fastest                 — speed leaderboard (avg seconds to correct answer)
  !help                    — show this help

Admin commands:
  !startgeo / !geoguessr          — start a round right now
  !cancelgeo                      — abort the current round (join phase or active)
  !cancelgeo HH:MM                — cancel a pending one-time game
  !schedulegeo HH:MM              — schedule a one-time game (join prompt fires earlier)
  !schedulegeo HH:MM reminder=N   — override join-prompt lead time (seconds)
  !schedulegeo HH:MM timeout=N    — override answer window (seconds)
  !schedulegeo                    — list pending one-time games
  !prefetch                       — manually pre-fetch images into the cache
  !resetstats confirm             — wipe all game history and reset the leaderboard

During a game, reply in the private chat with your location guess.
Any format works: city, country, full address, or lat,lon coordinates."
        .to_owned()
}

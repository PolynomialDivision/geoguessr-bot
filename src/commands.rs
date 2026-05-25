use anyhow::{anyhow, Result};
use matrix_sdk::ruma::OwnedUserId;
use tracing::error;

use crate::{BotContext, game};

pub async fn handle(ctx: &BotContext, sender: &OwnedUserId, body: &str) -> Result<Option<String>> {
    let cmd = body.split_whitespace().next().unwrap_or("").to_lowercase();

    match cmd.as_str() {
        "!startgeo"
        | "!geoguessr"   => cmd_startgeo(ctx, sender).await,
        "!prefetch"      => cmd_prefetch(ctx, sender).await,
        "!resetstats"    => cmd_resetstats(ctx, sender, body).await,
        "!scores"
        | "!leaderboard" => cmd_scores(ctx).await,
        "!mystats"       => cmd_mystats(ctx, sender).await,
        "!countries"     => cmd_countries(ctx).await,
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
    tokio::spawn(async move {
        if let Err(e) = game::start_round(ctx2, client, true, None).await {
            error!("Manual game error: {e}");
        }
    });

    Ok(Some(format!(
        "🌍 GeoGuessr starting! {} images, {}s per image.",
        ctx.config.schedule.images_per_round,
        ctx.config.schedule.answer_timeout_secs,
    )))
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

// ── !help ─────────────────────────────────────────────────────────────────────

fn help_text() -> String {
    "🌍 GeoGuessr Bot commands:

  !scores / !leaderboard   — overall ranking of all players
  !mystats                 — your personal score, rank and best/worst country
  !countries               — bar chart of every country shown with accuracy
  !fastest                 — speed leaderboard (avg seconds to correct answer)
  !help                    — show this help

Admin commands:
  !startgeo / !geoguessr   — start a round right now
  !prefetch                — manually pre-fetch images into the cache
  !resetstats confirm      — wipe all game history and reset the leaderboard

During a game, submit your answer:
  • React with 🇦 🇧 🇨 🇩
  • Or type  !a  !b  !c  !d
You can change your answer at any time before the timer runs out."
        .to_owned()
}

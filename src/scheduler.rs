use chrono::Timelike;
use chrono_tz::Tz;
use matrix_sdk::Client;
use tracing::{error, info, warn};

use crate::{BotContext, config::ScheduleConfig, game::GameOverrides, state::ScheduledOnce};

pub async fn run(ctx: BotContext, client: Client) {
    info!("GeoGuessr scheduler started");
    loop {
        if let Err(e) = tick(&ctx, &client).await {
            error!("Scheduler error: {e}");
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
    }
}

async fn tick(ctx: &BotContext, client: &Client) -> anyhow::Result<()> {
    let tz: Tz     = ctx.config.schedule.timezone.parse().unwrap_or(chrono_tz::UTC);
    let local_now  = chrono::Utc::now().with_timezone(&tz);
    let local_date = local_now.date_naive();
    let now_hour   = local_now.hour();
    let now_minute = local_now.minute();
    let offset     = ctx.config.schedule.reminder_before_secs as i64;

    // Guard: at most one round may be spawned per tick.
    //
    // active_game is only set inside play_free_guess (after the join-phase wait),
    // so checking it here is not sufficient to prevent two concurrent spawns within
    // the same tick — e.g. a recurring slot and a one-time slot resolving to the
    // same wall-clock minute.  This flag closes that window.
    let mut round_spawned = false;

    // ── Recurring slots from config ───────────────────────────────────────────
    for time_str in &ctx.config.schedule.game_times {
        let (qh, qm) = match ScheduleConfig::parse_game_time(time_str) {
            Some(t) => t,
            None => {
                warn!("Invalid game_times entry {:?} — skipping", time_str);
                continue;
            }
        };

        let game_secs  = (qh * 3600 + qm * 60) as i64;
        let fire_secs  = (game_secs - offset).rem_euclid(86400);
        let fire_hour  = (fire_secs / 3600) as u32;
        let fire_min   = ((fire_secs % 3600) / 60) as u32;

        if now_hour != fire_hour || now_minute != fire_min {
            continue;
        }

        {
            let state = ctx.state.lock().await;
            if state.last_game_dates.get(time_str.as_str()) == Some(&local_date) {
                continue;
            }
        }

        {
            let ag = ctx.active_game.lock().await;
            if ag.is_some() {
                warn!("Scheduler: fire time for {time_str} but a game is already running");
                continue;
            }
        }

        if round_spawned {
            warn!("Scheduler: fire time for {time_str} skipped — another round was already spawned this tick");
            continue;
        }

        // Mark the slot as done *before* spawning so the next tick (60 s later)
        // doesn't re-fire it if the round is still in its join phase and
        // active_game has not been set yet.
        {
            let mut state = ctx.state.lock().await;
            state.last_game_dates.insert(time_str.clone(), local_date);
            state.save(&ctx.state_path).await.ok();
        }

        info!("Scheduled game firing for slot {time_str}");
        round_spawned = true;
        let ctx2    = ctx.clone();
        let client2 = client.clone();
        let slot    = time_str.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = crate::game::start_round(ctx2, client2, false, Some(slot), None).await {
                error!("Game error: {e}");
            }
        });
        *ctx.round_abort.lock().await = Some(handle.abort_handle());
    }

    // ── One-time games (!schedulegeo) ─────────────────────────────────────────
    let once_entries: Vec<ScheduledOnce> = ctx.state.lock().await.scheduled_once.clone();

    for entry in once_entries {
        if entry.date != local_date { continue; }

        let (qh, qm) = match ScheduleConfig::parse_game_time(&entry.game_time) {
            Some(t) => t,
            None    => {
                warn!("Invalid scheduled_once time {:?} — removing", entry.game_time);
                let mut state = ctx.state.lock().await;
                state.scheduled_once.retain(|e| e != &entry);
                state.save(&ctx.state_path).await.ok();
                continue;
            }
        };

        let reminder = entry.reminder_before_secs
            .unwrap_or(ctx.config.schedule.reminder_before_secs);
        let game_secs  = (qh * 3600 + qm * 60) as i64;
        let fire_secs  = (game_secs - reminder as i64).rem_euclid(86400);
        let fire_hour  = (fire_secs / 3600) as u32;
        let fire_min   = ((fire_secs % 3600) / 60) as u32;

        if now_hour != fire_hour || now_minute != fire_min { continue; }

        // Remove before spawning so a restart can't double-fire.
        {
            let mut state = ctx.state.lock().await;
            state.scheduled_once.retain(|e| e != &entry);
            state.save(&ctx.state_path).await.ok();
        }

        {
            let ag = ctx.active_game.lock().await;
            if ag.is_some() {
                warn!(
                    "One-time game at {} would fire now but a game is already running — dropped",
                    entry.game_time,
                );
                continue;
            }
        }

        if round_spawned {
            warn!(
                "One-time game at {} dropped — another round was already spawned this tick",
                entry.game_time,
            );
            continue;
        }

        info!("One-time game firing for {} (fire at {fire_hour}:{fire_min:02})", entry.game_time);
        round_spawned = true;
        let ctx2    = ctx.clone();
        let client2 = client.clone();
        let overrides = GameOverrides {
            reminder_before_secs: entry.reminder_before_secs,
            answer_timeout_secs:  entry.answer_timeout_secs,
            guesses_per_round:    entry.guesses_per_round,
        };
        let handle = tokio::spawn(async move {
            if let Err(e) = crate::game::start_round(ctx2, client2, false, None, Some(overrides)).await {
                error!("One-time game error: {e}");
            }
        });
        *ctx.round_abort.lock().await = Some(handle.abort_handle());
    }

    Ok(())
}

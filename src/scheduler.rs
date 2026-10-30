use chrono::{NaiveDate, NaiveDateTime, Timelike};
use matrix_sdk::Client;
use mxbot_common::{matrix_sdk, supervise::Alive};
use tracing::{error, info, warn};

use crate::{config::ScheduleConfig, game::GameOverrides, state::ScheduledOnce, BotContext};

pub async fn run(ctx: BotContext, client: Client, alive: Alive) {
    info!("GeoGuessr scheduler started");
    loop {
        alive.tick();
        if let Err(e) = tick(&ctx, &client).await {
            error!("Scheduler error: {e}");
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
    }
}

/// How late a tick may still fire a slot: the loop sleeps a minute *after*
/// each tick, so now and then one skips a minute, and a restart takes a
/// while. Matching the exact minute lost the game on those days.
const CATCH_UP_SECS: i64 = 5 * 60;

/// When a game whose join message fires at `fire_secs` after local midnight
/// is due, as of local `now`.
#[derive(Debug, PartialEq)]
enum Due {
    /// Fire now: it was due on this (local) date, at most `CATCH_UP_SECS` ago.
    Now(NaiveDate),
    /// Not yet, or long past.
    No,
}

fn due(now: NaiveDateTime, fire_secs: i64) -> Due {
    // Whole minutes, as the slots were always matched.
    let fire_secs = fire_secs.rem_euclid(86400) / 60 * 60;
    let now_secs = i64::from(now.time().num_seconds_from_midnight());
    let late = now_secs - fire_secs;
    let (late, date) = if late >= 0 {
        (late, now.date())
    } else {
        // Fired just before midnight, yesterday.
        (late + 86400, now.date().pred_opt().unwrap_or(now.date()))
    };
    if late < CATCH_UP_SECS {
        Due::Now(date)
    } else {
        Due::No
    }
}

/// Whether a game due on `date` at `fire_secs` is past its catch-up window.
fn missed(now: NaiveDateTime, date: NaiveDate, fire_secs: i64) -> bool {
    let fire = date.and_time(chrono::NaiveTime::MIN)
        + chrono::Duration::seconds(fire_secs.rem_euclid(86400) / 60 * 60);
    (now - fire).num_seconds() >= CATCH_UP_SECS
}

async fn tick(ctx: &BotContext, client: &Client) -> anyhow::Result<()> {
    let local_now = chrono::Utc::now().with_timezone(&ctx.config.schedule.timezone);
    let local_date = local_now.date_naive();
    let now = local_now.naive_local();
    let offset = ctx.config.schedule.reminder_before_secs as i64;

    // Guard: at most one round may be spawned per tick.
    //
    // active_game is only set inside play_free_guess (after the join-phase wait),
    // so checking it here is not sufficient to prevent two concurrent spawns within
    // the same tick, e.g. a recurring slot and a one-time slot resolving to the
    // same wall-clock minute.  This flag closes that window.
    let mut round_spawned = false;

    // ── Recurring slots from config ───────────────────────────────────────────
    for game_time in &ctx.config.schedule.game_times {
        let (qh, qm) = (game_time.hour, game_time.minute);
        // The slot's key in `last_game_dates`, as "HH:MM".
        let time_str = &game_time.to_string();

        let game_secs = (qh * 3600 + qm * 60) as i64;
        let Due::Now(fire_date) = due(now, game_secs - offset) else {
            continue;
        };

        {
            let state = ctx.state.lock().await;
            if state.last_game_dates.get(time_str.as_str()) == Some(&fire_date) {
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
            warn!("Scheduler: fire time for {time_str} skipped, another round was already spawned this tick");
            continue;
        }

        // Mark the slot as done *before* spawning so the next tick (60 s later)
        // doesn't re-fire it if the round is still in its join phase and
        // active_game has not been set yet.
        {
            let mut state = ctx.state.lock().await;
            state.last_game_dates.insert(time_str.clone(), fire_date);
            state.save(&ctx.state_path).await.ok();
        }

        info!("Scheduled game firing for slot {time_str}");
        round_spawned = true;
        let ctx2 = ctx.clone();
        let client2 = client.clone();
        let slot = time_str.clone();
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
        if entry.date > local_date {
            continue;
        }

        let (qh, qm) = match ScheduleConfig::parse_game_time(&entry.game_time) {
            Some(t) => t,
            None => {
                warn!(
                    "Invalid scheduled_once time {:?}, removing",
                    entry.game_time
                );
                let mut state = ctx.state.lock().await;
                state.scheduled_once.retain(|e| e != &entry);
                state.save(&ctx.state_path).await.ok();
                continue;
            }
        };

        let reminder = entry
            .reminder_before_secs
            .unwrap_or(ctx.config.schedule.reminder_before_secs);
        let game_secs = (qh * 3600 + qm * 60) as i64;
        let fire_secs = (game_secs - reminder as i64).rem_euclid(86400);
        let fire_hour = (fire_secs / 3600) as u32;
        let fire_min = ((fire_secs % 3600) / 60) as u32;

        if due(now, fire_secs) != Due::Now(entry.date) {
            if missed(now, entry.date, fire_secs) {
                // The bot was down then: drop it, or it's listed forever.
                warn!(
                    "One-time game at {} on {} was missed, removing",
                    entry.game_time, entry.date
                );
                let mut state = ctx.state.lock().await;
                state.scheduled_once.retain(|e| e != &entry);
                state.save(&ctx.state_path).await.ok();
            }
            continue;
        }

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
                    "One-time game at {} would fire now but a game is already running: dropped",
                    entry.game_time,
                );
                continue;
            }
        }

        if round_spawned {
            warn!(
                "One-time game at {} dropped: another round was already spawned this tick",
                entry.game_time,
            );
            continue;
        }

        info!(
            "One-time game firing for {} (fire at {fire_hour}:{fire_min:02})",
            entry.game_time
        );
        round_spawned = true;
        let ctx2 = ctx.clone();
        let client2 = client.clone();
        let overrides = GameOverrides {
            reminder_before_secs: entry.reminder_before_secs,
            answer_timeout_secs: entry.answer_timeout_secs,
            guesses_per_round: entry.guesses_per_round,
        };
        let handle = tokio::spawn(async move {
            if let Err(e) =
                crate::game::start_round(ctx2, client2, false, None, Some(overrides)).await
            {
                error!("One-time game error: {e}");
            }
        });
        *ctx.round_abort.lock().await = Some(handle.abort_handle());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: u32, h: u32, m: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, day)
            .unwrap()
            .and_hms_opt(h, m, s)
            .unwrap()
    }

    #[test]
    fn a_slot_is_due_from_its_minute_for_the_catch_up_window() {
        let nine = 9 * 3600;
        assert_eq!(due(at(22, 8, 59, 59), nine), Due::No);
        assert_eq!(due(at(22, 9, 0, 0), nine), Due::Now(at(22, 0, 0, 0).date()));
        // A tick that skipped the minute, or a restart, still fires it.
        assert_eq!(due(at(22, 9, 1, 0), nine), Due::Now(at(22, 0, 0, 0).date()));
        assert_eq!(
            due(at(22, 9, 4, 59), nine),
            Due::Now(at(22, 0, 0, 0).date())
        );
        assert_eq!(due(at(22, 9, 5, 0), nine), Due::No);
        // Seconds in the fire time (a trimmed reminder) count from its minute.
        assert_eq!(
            due(at(22, 9, 0, 10), nine + 30),
            Due::Now(at(22, 0, 0, 0).date())
        );
    }

    #[test]
    fn a_slot_just_before_midnight_counts_for_its_own_day() {
        let fire = 23 * 3600 + 58 * 60;
        assert_eq!(
            due(at(22, 23, 58, 30), fire),
            Due::Now(at(22, 0, 0, 0).date())
        );
        assert_eq!(due(at(23, 0, 1, 0), fire), Due::Now(at(22, 0, 0, 0).date()));
        assert_eq!(due(at(23, 0, 4, 0), fire), Due::No);
        // A negative fire time (reminder before midnight) wraps to the day before.
        assert_eq!(
            due(at(22, 23, 0, 0), -3600),
            Due::Now(at(22, 0, 0, 0).date())
        );
    }

    #[test]
    fn a_one_time_game_is_missed_after_its_window() {
        let date = at(22, 0, 0, 0).date();
        let nine = 9 * 3600;
        assert!(!missed(at(22, 8, 0, 0), date, nine));
        assert!(!missed(at(22, 9, 4, 0), date, nine));
        assert!(missed(at(22, 9, 5, 0), date, nine));
        assert!(missed(at(23, 8, 0, 0), date, nine));
    }
}

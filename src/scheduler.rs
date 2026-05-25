use chrono::Timelike;
use chrono_tz::Tz;
use matrix_sdk::Client;
use tracing::{error, info, warn};

use crate::{BotContext, config::ScheduleConfig};

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

        info!("Scheduled game firing for slot {time_str}");
        let ctx2    = ctx.clone();
        let client2 = client.clone();
        let slot    = time_str.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::game::start_round(ctx2, client2, false, Some(slot)).await {
                error!("Game error: {e}");
            }
        });
    }

    Ok(())
}

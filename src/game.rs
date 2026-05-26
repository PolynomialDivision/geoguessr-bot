//! Round and image game logic.

use std::collections::{HashMap, HashSet};

use anyhow::Context as _;
use chrono_tz::Tz;
use matrix_sdk::{
    Client, Room,
    ruma::{
        OwnedEventId, OwnedRoomId, OwnedUserId,
        events::{
            Mentions,
            reaction::ReactionEventContent,
            relation::Annotation,
            room::message::{
                ImageMessageEventContent, MessageType,
                ReplacementMetadata, RoomMessageEventContent,
            },
        },
    },
};
use rand::seq::SliceRandom;
use tracing::{error, info, warn};

use crate::{
    BotContext,
    config::GameMode,
    countries,
    db::AnswerRecord,
    format,
    sources::GeoImage,
    state::PendingJoin,
};

// ── Per-round overrides ───────────────────────────────────────────────────────

/// Optional overrides for a single game round, used by `!schedulegeo`.
pub struct GameOverrides {
    /// Override for how long before game time the join message fires (seconds).
    pub reminder_before_secs: Option<u64>,
    /// Override for how long players have to answer (seconds).
    pub answer_timeout_secs:  Option<u64>,
}

pub const CHOICE_EMOJIS: [&str; 4] = ["🇦", "🇧", "🇨", "🇩"];

// ── Free-guess answer ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct FreeGuess {
    pub text:         String,
    pub lat:          f64,
    pub lon:          f64,
    pub submitted_at: chrono::DateTime<chrono::Utc>,
}

// ── Active game state (in-memory only) ────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ActiveGame {
    pub event_id: OwnedEventId,
    pub mode:     ActiveGameMode,
}

#[derive(Clone, Debug)]
pub enum ActiveGameMode {
    MultipleChoice {
        answers:       HashMap<String, AnswerRecord>,
        correct_index: u8,
    },
    FreeGuess {
        guesses:    HashMap<String, FreeGuess>,
        actual_lat: f64,
        actual_lon: f64,
    },
}

impl ActiveGame {
    pub fn record_mc_answer(&mut self, user_id: String, choice: u8, source: &'static str) {
        let now = chrono::Utc::now();
        if let ActiveGameMode::MultipleChoice { ref mut answers, .. } = self.mode {
            answers
                .entry(user_id)
                .and_modify(|r| {
                    let changed = r.choice != choice;
                    r.choice       = choice;
                    r.source       = source;
                    r.submitted_at = now;
                    if changed { r.changed_answer = true; }
                })
                .or_insert(AnswerRecord {
                    choice,
                    source,
                    submitted_at:   now,
                    changed_answer: false,
                });
        }
    }

    pub fn record_free_guess(&mut self, user_id: String, guess: FreeGuess) {
        if let ActiveGameMode::FreeGuess { ref mut guesses, .. } = self.mode {
            guesses.insert(user_id, guess);
        }
    }
}

// ── Round entry point ─────────────────────────────────────────────────────────

pub async fn start_round(
    ctx:       BotContext,
    client:    Client,
    manual:    bool,
    slot:      Option<String>,
    overrides: Option<GameOverrides>,
) -> anyhow::Result<()> {
    let room = match client.get_room(&ctx.room_id) {
        Some(r) => r,
        None    => {
            warn!("GeoGuessr: room {} not joined", ctx.room_id);
            return Ok(());
        }
    };

    let n = ctx.config.schedule.guesses_per_round as usize;
    let triggered_by = if manual { "manual" } else { "scheduler" };

    // Apply per-round overrides (from !schedulegeo).
    let reminder_before_secs_cfg = overrides.as_ref()
        .and_then(|o| o.reminder_before_secs)
        .unwrap_or(ctx.config.schedule.reminder_before_secs);
    let answer_timeout_secs_cfg = overrides.as_ref()
        .and_then(|o| o.answer_timeout_secs)
        .unwrap_or(ctx.config.schedule.answer_timeout_secs);

    // ── Join phase (free-guess + scheduled only) ──────────────────────────────
    // When reminder_before_secs > 0 and game_mode == FreeGuess, post a
    // "who wants to play?" message, react to it, wait for participants,
    // then open a DM with each opt-in player.
    let dm_participants: HashMap<OwnedUserId, OwnedRoomId> =
        if !manual
            && ctx.config.schedule.game_mode == GameMode::FreeGuess
            && reminder_before_secs_cfg > 0
        {
            let reminder_secs = reminder_before_secs_cfg;
            let emoji         = ctx.config.schedule.join_emoji.clone();

            // Post the join-prompt message.
            let flags_str = JOIN_REACTION_FLAGS.join(" ");
            let join_msg = format!(
                "🌍 GeoGuessr starts in {}! @room\n\
                 I'll invite you to a private chat · answer there.\n\
                 React with your flag to join and set the map language:\n\
                 {}",
                format_duration(reminder_secs),
                flags_str,
            );
            let mut join_content = format::mentionify(&join_msg);
            join_content = join_content.add_mentions({
                let mut m = Mentions::new();
                m.room = true;
                m
            });
            let join_event    = room.send(join_content).await?;
            let join_event_id = join_event.response.event_id.clone();

            // Bot primes the flag reactions so clients show them as tappable buttons.
            for flag in JOIN_REACTION_FLAGS {
                room.send(ReactionEventContent::new(Annotation::new(
                    join_event_id.clone(),
                    flag.to_string(),
                )))
                .await
                .ok();
            }

            // Register the join event so the reaction handler populates participants.
            {
                let mut js = ctx.join_state.lock().await;
                js.message_event_id = Some(join_event_id.clone());
                js.join_emoji       = emoji.clone();
                js.participants.clear();
            }

            // Persist the join phase so a restart can resume it.
            {
                let game_at_utc = chrono::Utc::now()
                    + chrono::Duration::seconds(reminder_secs as i64);
                let mut st = ctx.state.lock().await;
                st.pending_join = Some(PendingJoin {
                    event_id:           join_event_id.to_string(),
                    join_emoji:         emoji.clone(),
                    slot:               slot.clone(),
                    game_at_utc,
                    answer_timeout_secs: answer_timeout_secs_cfg,
                });
                st.save(&ctx.state_path).await.ok();
            }

            // Wait for the join window.
            tokio::time::sleep(tokio::time::Duration::from_secs(reminder_secs)).await;

            // Collect participants from in-memory tracking …
            let mut participants: HashSet<OwnedUserId> = {
                let mut js = ctx.join_state.lock().await;
                js.message_event_id = None;
                std::mem::take(&mut js.participants)
            };

            // … and also reconcile with the server in case we missed any reactions.
            let bot_uid = client.user_id().map(|u| u.to_owned());
            reconcile_join_reactions(
                &client,
                &room,
                &join_event_id,
                bot_uid.as_deref(),
                &mut participants,
            )
            .await;

            // Clear pending_join — join window is over.
            {
                let mut st = ctx.state.lock().await;
                st.pending_join = None;
                st.save(&ctx.state_path).await.ok();
            }

            if participants.is_empty() {
                room.send(format::mentionify(
                    "😴 Nobody joined · skipping.",
                ))
                .await
                .ok();
                // Mark slot as done so we don't retry.
                if let Some(slot) = slot {
                    let tz: Tz = ctx.config.schedule.timezone.parse().unwrap_or(chrono_tz::UTC);
                    let today  = chrono::Utc::now().with_timezone(&tz).date_naive();
                    let mut st = ctx.state.lock().await;
                    st.last_game_dates.insert(slot, today);
                    st.save(&ctx.state_path).await.ok();
                }
                return Ok(());
            }

            // Open (or reuse) a DM with each participant.
            let mut dm_map: HashMap<OwnedUserId, OwnedRoomId> = HashMap::new();
            for uid in &participants {
                match get_or_create_dm(&client, uid).await {
                    Ok(dm_room_id) => {
                        // Register in the global DM-room map so the message handler
                        // can route answers.
                        ctx.dm_rooms.lock().await
                            .insert(dm_room_id.clone(), uid.clone());

                        dm_map.insert(uid.clone(), dm_room_id);
                    }
                    Err(e) => warn!("Could not open DM with {uid}: {e}"),
                }
            }

            let participant_list: Vec<String> = dm_map.keys()
                .map(|u| u.to_string())
                .collect();
            let participant_ids: Vec<OwnedUserId> = dm_map.keys().cloned().collect();
            room.send(
                format::mentionify(&format!(
                    "🌍 GeoGuessr starting now! {} player{}: {}",
                    dm_map.len(),
                    if dm_map.len() == 1 { "" } else { "s" },
                    participant_list.join(", "),
                ))
                .add_mentions(Mentions::with_user_ids(participant_ids)),
            )
            .await
            .ok();

            dm_map

        } else {
            // Scheduled with reminder but not free-guess, or manual game:
            // classic main-room flow with the old "starting soon" message.
            if !manual {
                let reminder_secs = reminder_before_secs_cfg;
                if reminder_secs > 0 {
                    tokio::time::sleep(tokio::time::Duration::from_secs(reminder_secs)).await;
                }
            }
            HashMap::new()
        };

    // ── Pre-fetch images ──────────────────────────────────────────────────────
    prefetch_if_needed(&ctx, n).await;

    let round_id = ctx.db
        .start_round(ctx.room_id.as_str(), n as u32, triggered_by)
        .await?;

    info!("GeoGuessr round {round_id} started ({n} images, triggered_by={triggered_by})");

    let mut round_results: HashMap<String, Vec<bool>> = HashMap::new();
    let mut round_scores_free: HashMap<String, i64>   = HashMap::new();

    for i in 0..n {
        let img = {
            let mut st = ctx.state.lock().await;
            match st.cached_guesses.pop_front() {
                Some(img) => {
                    st.save(&ctx.state_path).await.ok();
                    img
                }
                None => {
                    warn!("GeoGuessr: guess cache empty — skipping remaining");
                    break;
                }
            }
        };

        // Background refill.
        {
            let remaining = ctx.state.lock().await.cached_guesses.len();
            if remaining < 3 {
                let ctx2 = ctx.clone();
                tokio::spawn(async move {
                    prefetch_if_needed(&ctx2, 3).await;
                });
            }
        }

        if i > 0 {
            tokio::time::sleep(tokio::time::Duration::from_secs(
                ctx.config.schedule.inter_guess_secs,
            ))
            .await;
        }

        match ctx.config.schedule.game_mode {
            GameMode::MultipleChoice => {
                play_guess(
                    &ctx, &client, &room,
                    round_id, i as u32 + 1, n as u32,
                    &img, &mut round_results,
                    answer_timeout_secs_cfg,
                )
                .await?;
            }
            GameMode::FreeGuess => {
                play_free_guess(
                    &ctx, &client, &room,
                    round_id, i as u32 + 1, n as u32,
                    &img, &mut round_scores_free, &dm_participants,
                    answer_timeout_secs_cfg,
                )
                .await?;
            }
        }
    }

    // ── Finalise round ────────────────────────────────────────────────────────
    ctx.db.finish_round(round_id).await?;
    match ctx.config.schedule.game_mode {
        GameMode::MultipleChoice => {
            ctx.db.upsert_round_scores(round_id, &round_results).await?;
        }
        GameMode::FreeGuess => {
            ctx.db.upsert_round_scores_free_guess(round_id, &round_scores_free).await?;
        }
    }

    if let Some(slot) = slot {
        let tz: Tz = ctx.config.schedule.timezone.parse().unwrap_or(chrono_tz::UTC);
        let today  = chrono::Utc::now().with_timezone(&tz).date_naive();
        let mut st = ctx.state.lock().await;
        st.last_game_dates.insert(slot, today);
        st.save(&ctx.state_path).await.ok();
    }

    // Post round summary.
    match ctx.config.schedule.game_mode {
        GameMode::MultipleChoice => {
            post_round_summary(&ctx, &client, &room, round_id, &round_results).await;
        }
        GameMode::FreeGuess => {
            post_round_summary_free_guess(
                &ctx, &client, &room,
                round_id, &round_scores_free, &dm_participants,
            )
            .await;
        }
    }

    // Clear DM-room mappings after the round ends so stale rooms don't
    // absorb future messages.
    if !dm_participants.is_empty() {
        let dm_room_ids: Vec<OwnedRoomId> = dm_participants.values().cloned().collect();
        let mut dm_rooms = ctx.dm_rooms.lock().await;
        for id in &dm_room_ids {
            dm_rooms.remove(id);
        }
    }

    Ok(())
}

// ── Single image — multiple choice ───────────────────────────────────────────

async fn play_guess(
    ctx:                &BotContext,
    client:             &Client,
    room:               &Room,
    round_id:           i64,
    guess_num:          u32,
    n_total:            u32,
    img:                &GeoImage,
    round_results:      &mut HashMap<String, Vec<bool>>,
    answer_timeout_secs: u64,
) -> anyhow::Result<()> {
    let mut distractors = countries::pick_distractors(&img.country, &img.region, 3);
    distractors.push(img.country.clone());
    distractors.shuffle(&mut rand::thread_rng());
    let correct_index = distractors
        .iter()
        .position(|c| c == &img.country)
        .unwrap_or(0) as u8;
    let choices = distractors;

    let guess_id = ctx.db
        .start_guess(
            round_id, guess_num,
            &img.country, &img.region, img.city.as_deref(),
            &img.source, img.attribution.as_deref(),
            &choices, correct_index,
            answer_timeout_secs,
            img.lat, img.lon,
        )
        .await?;

    let all_images = match upload_all_images(client, img).await {
        Ok(v)  => v,
        Err(e) => {
            error!("GeoGuessr: failed to upload image: {e}");
            return Ok(());
        }
    };

    let n_imgs = all_images.len();
    for (i, (mxc_uri, _mime)) in all_images.into_iter().enumerate() {
        let label = if n_imgs == 1 {
            "📍 Where is this?".to_owned()
        } else {
            format!("📍 {}/{}", i + 1, n_imgs)
        };
        let image_content = ImageMessageEventContent::plain(label, mxc_uri);
        room.send(RoomMessageEventContent::new(MessageType::Image(image_content))).await?;
    }

    let total_secs    = answer_timeout_secs;
    let question_text = build_question_text(guess_num, n_total, &choices, total_secs, total_secs);
    // @room on the first question of every round so people get a push notification.
    // Subsequent questions in the same round don't need it — players are already watching.
    let initial_text = if guess_num == 1 {
        format!("@room\n{question_text}")
    } else {
        question_text
    };
    let mut q_content = format::mentionify(&initial_text);
    if guess_num == 1 {
        let mut m = Mentions::new();
        m.room = true;
        q_content = q_content.add_mentions(m);
    }
    let q_event       = room.send(q_content).await?;
    let q_event_id    = q_event.response.event_id.clone();

    ctx.db.set_guess_event_id(guess_id, q_event_id.as_str()).await.ok();

    for emoji in &CHOICE_EMOJIS[..choices.len()] {
        room.send(ReactionEventContent::new(Annotation::new(
            q_event_id.clone(),
            emoji.to_string(),
        )))
        .await
        .ok();
    }

    {
        let mut ag = ctx.active_game.lock().await;
        *ag = Some(ActiveGame {
            event_id: q_event_id.clone(),
            mode: ActiveGameMode::MultipleChoice {
                answers:       HashMap::new(),
                correct_index,
            },
        });
    }

    // Smooth countdown: divide the total into ~20 ticks, min 15 s, max 15 min.
    let edit_interval = (total_secs / 20).clamp(15, 900);
    let mut remaining = total_secs;
    while remaining > edit_interval {
        tokio::time::sleep(tokio::time::Duration::from_secs(edit_interval)).await;
        remaining -= edit_interval;
        let updated = build_question_text(guess_num, n_total, &choices, remaining, total_secs);
        if let Some(r) = client.get_room(&ctx.room_id) {
            let edit = RoomMessageEventContent::text_plain(&updated)
                .make_replacement(ReplacementMetadata::new(q_event_id.clone(), None));
            r.send(edit).await.ok();
        }
    }
    tokio::time::sleep(tokio::time::Duration::from_secs(remaining)).await;

    let mut answers = {
        let mut ag = ctx.active_game.lock().await;
        match ag.take().map(|g| g.mode) {
            Some(ActiveGameMode::MultipleChoice { answers, .. }) => answers,
            _ => HashMap::new(),
        }
    };
    reconcile_reactions(client, room, &q_event_id, &mut answers).await;

    let n_correct = answers.values().filter(|r| r.choice == correct_index).count();
    let n_total_a = answers.len();

    ctx.db.record_answers(guess_id, round_id, answers.clone(), correct_index).await?;
    ctx.db.finish_guess(guess_id, n_total_a, n_correct).await?;

    for (user_id, rec) in &answers {
        round_results
            .entry(user_id.clone())
            .or_default()
            .push(rec.choice == correct_index);
    }

    post_reveal(ctx, client, room, img, &choices, correct_index, &answers, n_correct).await;
    Ok(())
}

// ── Single image — free guess ─────────────────────────────────────────────────

async fn play_free_guess(
    ctx:                &BotContext,
    client:             &Client,
    room:               &Room,
    round_id:           i64,
    guess_num:          u32,
    n_total:            u32,
    img:                &GeoImage,
    round_scores:       &mut HashMap<String, i64>,
    dm_participants:    &HashMap<OwnedUserId, OwnedRoomId>,
    answer_timeout_secs: u64,
) -> anyhow::Result<()> {
    let (actual_lat, actual_lon) = match (img.lat, img.lon) {
        (Some(lat), Some(lon)) => (lat, lon),
        _ => {
            countries::COUNTRIES.iter()
                .find(|c| c.name == img.country)
                .map(|c| (c.lat, c.lon))
                .unwrap_or((0.0, 0.0))
        }
    };

    let guess_id = ctx.db
        .start_guess(
            round_id, guess_num,
            &img.country, &img.region, img.city.as_deref(),
            &img.source, img.attribution.as_deref(),
            &[], 0,
            answer_timeout_secs,
            Some(actual_lat), Some(actual_lon),
        )
        .await?;

    // Upload all images once; reuse mxc_uris across main room + all DMs.
    let all_images = match upload_all_images(client, img).await {
        Ok(v)  => v,
        Err(e) => {
            error!("GeoGuessr free-guess: failed to upload image: {e}");
            return Ok(());
        }
    };

    let n_imgs = all_images.len();

    // Post all images to the main room.
    for (i, (mxc_uri, _mime)) in all_images.iter().enumerate() {
        let label = if n_imgs == 1 {
            "📍 Where is this?".to_owned()
        } else {
            format!("📍 {}/{}", i + 1, n_imgs)
        };
        let image_content = ImageMessageEventContent::plain(label, mxc_uri.clone());
        room.send(RoomMessageEventContent::new(MessageType::Image(image_content))).await?;
    }

    let total_secs  = answer_timeout_secs;
    let timeout_str = format_duration(total_secs);
    let bot_mxid    = client.user_id().map(|u| u.to_string()).unwrap_or_default();
    let room_id_str = ctx.room_id.as_str();

    // Show the "can't find the chat?" hint only once per round, below the first batch of photos.
    // Plain variant: used in countdown edits (plain-text). HTML variant: used in the initial send.
    let q_event = if dm_participants.is_empty() {
        room.send(format::mentionify(&format!(
            "🌍 Guess {guess_num}/{n_total} | ⏳ {timeout_str}\n\
             📍 Where is this? Type: **!guess** city, country, or lat,lon",
        ))).await?
    } else if guess_num == 1 {
        let plain = format!("🌍 Guess {guess_num}/{n_total} | ⏳ {timeout_str} | 💬 Open DM");
        let html  = format!(
            "🌍 Guess {guess_num}/{n_total} | ⏳ {timeout_str} | \
             💬 <a href=\"https://matrix.to/#/{bot_mxid}\">Open DM</a>"
        );
        room.send(RoomMessageEventContent::text_html(plain, html)).await?
    } else {
        room.send(format::mentionify(&format!(
            "🌍 Guess {guess_num}/{n_total} | ⏳ {timeout_str}",
        ))).await?
    };
    let q_event_id = q_event.response.event_id.clone();

    ctx.db.set_guess_event_id(guess_id, q_event_id.as_str()).await.ok();

    // Post all images + prompt in each participant's DM.
    let dm_prompt_plain = format!(
        "🌍 Guess {guess_num}/{n_total} | ⏳ {timeout_str}\n\
         📍 Where is this?\n\
         City, country, address, or lat,lon\n\
         ↩️ Main room: https://matrix.to/#/{room_id_str}"
    );
    for (uid, dm_room_id) in dm_participants.iter() {
        if let Some(dm_room) = client.get_room(dm_room_id) {
            for (i, (mxc_uri, _mime)) in all_images.iter().enumerate() {
                let label = if n_imgs == 1 {
                    "📍 Where is this?".to_owned()
                } else {
                    format!("📍 {}/{}", i + 1, n_imgs)
                };
                let dm_img = ImageMessageEventContent::plain(label, mxc_uri.clone());
                dm_room.send(RoomMessageEventContent::new(MessageType::Image(dm_img))).await.ok();
            }
            let lang = ctx.state.lock().await
                .user_langs.get(uid.as_str()).cloned()
                .unwrap_or_else(|| "en".to_owned());
            let dm_prompt_html = format!(
                "🌍 Guess {guess_num}/{n_total} | ⏳ {timeout_str}<br>\
                 📍 Where is this?<br>\
                 City, country, address, or \
                 <a href=\"https://polynomialdivision.github.io/geo-picker/?lang={lang}\">pick on map</a><br>\
                 ↩️ <a href=\"https://matrix.to/#/{room_id_str}\">Main room</a>"
            );
            dm_room.send(RoomMessageEventContent::text_html(&dm_prompt_plain, &dm_prompt_html)).await.ok();
        }
    }

    // Register active game.
    {
        let mut ag = ctx.active_game.lock().await;
        *ag = Some(ActiveGame {
            event_id: q_event_id.clone(),
            mode: ActiveGameMode::FreeGuess {
                guesses:    HashMap::new(),
                actual_lat,
                actual_lon,
            },
        });
    }

    // Smooth countdown in the main room — edits the prompt message with a time bar.
    // DMs are not updated (avoids spamming participants).
    let edit_interval = (total_secs / 20).clamp(15, 900);
    let mut remaining = total_secs;
    while remaining > edit_interval {
        tokio::time::sleep(tokio::time::Duration::from_secs(edit_interval)).await;
        remaining -= edit_interval;
        let bar       = time_bar(remaining, total_secs);
        let time_str  = format_duration(remaining);
        let edit_msg = if dm_participants.is_empty() {
            format::mentionify(&format!(
                "🌍 Guess {guess_num}/{n_total} | ⏳ {time_str}  {bar}\n\
                 📍 Where is this? Type: **!guess** city, country, or lat,lon",
            ))
        } else if guess_num == 1 {
            let plain = format!("🌍 Guess {guess_num}/{n_total} | ⏳ {time_str}  {bar} | 💬 Open DM");
            let html  = format!(
                "🌍 Guess {guess_num}/{n_total} | ⏳ {time_str}  {bar} | \
                 💬 <a href=\"https://matrix.to/#/{bot_mxid}\">Open DM</a>"
            );
            RoomMessageEventContent::text_html(plain, html)
        } else {
            format::mentionify(&format!(
                "🌍 Guess {guess_num}/{n_total} | ⏳ {time_str}  {bar}",
            ))
        };
        if let Some(r) = client.get_room(&ctx.room_id) {
            let edit = edit_msg
                .make_replacement(ReplacementMetadata::new(q_event_id.clone(), None));
            r.send(edit).await.ok();
        }
    }
    tokio::time::sleep(tokio::time::Duration::from_secs(remaining)).await;

    // Collect guesses.
    let guesses = {
        let mut ag = ctx.active_game.lock().await;
        match ag.take().map(|g| g.mode) {
            Some(ActiveGameMode::FreeGuess { guesses, .. }) => guesses,
            _ => HashMap::new(),
        }
    };

    // Score.
    let mut scored: Vec<(String, FreeGuess, f64, i64)> = guesses
        .into_iter()
        .map(|(uid, guess)| {
            let dist  = haversine_km(guess.lat, guess.lon, actual_lat, actual_lon);
            let score = distance_score(dist);
            (uid, guess, dist, score)
        })
        .collect();
    scored.sort_by(|a, b| b.3.cmp(&a.3));

    let n_answers = scored.len();

    let db_rows: Vec<(String, String, f64, f64, f64, i64)> = scored
        .iter()
        .map(|(uid, g, dist, score)| {
            (uid.clone(), g.text.clone(), g.lat, g.lon, *dist, *score)
        })
        .collect();
    ctx.db.record_free_guess_answers(guess_id, round_id, db_rows).await?;
    ctx.db.finish_guess(guess_id, n_answers, 0).await?;

    for (uid, _, _, score) in &scored {
        *round_scores.entry(uid.clone()).or_insert(0) += score;
    }

    let user_ids: Vec<&str> = scored.iter().map(|(uid, _, _, _)| uid.as_str()).collect();
    let names        = fetch_names(room, &user_ids).await;
    let raw_avatars  = crate::avatar::fetch_player_avatars(room, &user_ids).await;
    post_reveal_free_guess(
        ctx, client, img,
        actual_lat, actual_lon,
        &scored, &names, &raw_avatars, dm_participants,
    )
    .await;

    Ok(())
}

async fn post_reveal_free_guess(
    ctx:             &BotContext,
    client:          &Client,
    img:             &GeoImage,
    actual_lat:      f64,
    actual_lon:      f64,
    scored:          &[(String, FreeGuess, f64, i64)],
    names:           &HashMap<String, String>,
    raw_avatars:     &HashMap<String, Vec<u8>>,
    dm_participants: &HashMap<OwnedUserId, OwnedRoomId>,
) {
    let location_str = match &img.city {
        Some(city) => format!("{}, {}", city, img.country),
        None       => img.country.clone(),
    };

    let maps_url = format!(
        "https://www.openstreetmap.org/?mlat={:.4}&mlon={:.4}#map=8/{:.4}/{:.4}",
        actual_lat, actual_lon, actual_lat, actual_lon,
    );

    // ── Reverse-geocode each guess for human-readable labels ─────────────────
    let mut guess_labels:    Vec<String>                   = Vec::with_capacity(scored.len());
    let mut overview_entries: Vec<(&str, f64, f64, u8, u8, u8)> = Vec::with_capacity(scored.len());
    for (i, (uid, guess, _, _)) in scored.iter().enumerate() {
        let (r, g, b, _) = crate::mapimage::PLAYER_COLORS[i % crate::mapimage::PLAYER_COLORS.len()];
        let map_url = build_guess_map_url(
            guess.lat, guess.lon, actual_lat, actual_lon, r, g, b, "en",
        );
        let label = crate::geocode::reverse_geocode(guess.lat, guess.lon, "en")
            .await
            .unwrap_or_else(|| format!("{:.2}, {:.2}", guess.lat, guess.lon));
        guess_labels.push(format!("[{label}]({map_url})"));
        overview_entries.push((uid.as_str(), guess.lat, guess.lon, r, g, b));
    }

    // ── Build "all guesses" overview map URL ──────────────────────────────────
    let overview_url = if !overview_entries.is_empty() {
        build_all_guesses_map_url(&overview_entries, actual_lat, actual_lon, names)
    } else {
        maps_url.clone()
    };

    // ── Main-room reveal ──────────────────────────────────────────────────────
    let header_map_label = if scored.is_empty() { "Map" } else { "All guesses" };
    let mut lines = vec![
        format!("📍 **{}** [{header_map_label}]({})", location_str, overview_url),
        String::new(),
    ];

    if scored.is_empty() {
        lines.push("Nobody guessed.".to_owned());
    } else {
        for (i, ((uid, _guess, dist, score), guess_link)) in
            scored.iter().zip(guess_labels.iter()).enumerate()
        {
            let medal    = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "  " };
            let dist_str = format_dist(*dist);
            lines.push(format!(
                "{medal} {} · {guess_link} · {} · {} pts",
                uid, dist_str, score,
            ));
        }
    }
    let text = lines.join("\n");
    if let Some(r) = client.get_room(&ctx.room_id) {
        r.send(format::mentionify(&text)).await.ok();
    }

    // ── Main-room map images ──────────────────────────────────────────────────
    let map_mime: mime::Mime = "image/png".parse().unwrap();

    // 1. Winner's individual map (always shown when anyone guessed).
    if let Some((_, winner_guess, winner_dist, _)) = scored.first() {
        let (g_lat, g_lon, d) = (winner_guess.lat, winner_guess.lon, *winner_dist);
        if let Ok(Some(png)) = tokio::task::spawn_blocking(move || {
            crate::mapimage::render_guess_map(g_lat, g_lon, actual_lat, actual_lon, d)
        })
        .await
        {
            if let Ok(resp) = client.media().upload(&map_mime, png, None).await {
                let label = format!("🥇 Best guess · {} away", format_dist(d));
                let img = ImageMessageEventContent::plain(label, resp.content_uri);
                if let Some(r) = client.get_room(&ctx.room_id) {
                    r.send(RoomMessageEventContent::new(MessageType::Image(img))).await.ok();
                }
            }
        }
    }

    // 2. Combined round map — only when 2+ players guessed.
    if scored.len() >= 2 {
        // Collect (name, lat, lon, raw_avatar_bytes) — avatar rendering happens
        // inside spawn_blocking alongside the map rendering (both CPU-bound).
        let guess_data_raw: Vec<(String, f64, f64, Option<Vec<u8>>)> = scored
            .iter()
            .map(|(uid, guess, _, _)| {
                let raw = raw_avatars.get(uid.as_str()).cloned();
                (display_name(names, uid).to_owned(), guess.lat, guess.lon, raw)
            })
            .collect();

        if let Ok(Some((png, legend))) = tokio::task::spawn_blocking(move || {
            use crate::mapimage::PLAYER_COLORS;
            // Render each avatar into a pin PNG, then build the round map.
            let guess_pins: Vec<(String, f64, f64, Option<Vec<u8>>)> = guess_data_raw
                .into_iter()
                .enumerate()
                .map(|(idx, (name, lat, lon, raw))| {
                    let (pr, pg, pb, _) = PLAYER_COLORS[idx % PLAYER_COLORS.len()];
                    let pin = crate::avatar::render_avatar_pin(
                        raw.as_deref(), pr, pg, pb,
                    );
                    (name, lat, lon, pin)
                })
                .collect();
            crate::mapimage::render_round_map(&guess_pins, actual_lat, actual_lon)
        })
        .await
        {
            if let Ok(resp) = client.media().upload(&map_mime, png, None).await {
                let label = format!("🗺️ All {} guesses this round", scored.len());
                let img = ImageMessageEventContent::plain(label, resp.content_uri);
                if let Some(r) = client.get_room(&ctx.room_id) {
                    r.send(RoomMessageEventContent::new(MessageType::Image(img))).await.ok();

                    // Legend: 🔵 @alice:s  🔴 @bob:s  ⬛ actual
                    let mut parts: Vec<String> = scored
                        .iter()
                        .zip(legend.iter())
                        .map(|((uid, _, _, _), (_, emoji))| format!("{} {}", emoji, uid))
                        .collect();
                    parts.push("⬛ actual location".to_owned());
                    r.send(format::mentionify(&parts.join("   ")))
                        .await
                        .ok();
                }
            }
        }
    }

    // ── Per-user DM feedback ──────────────────────────────────────────────────
    for (rank_0, (uid, guess, dist, score)) in scored.iter().enumerate() {
        let rank  = rank_0 + 1;
        let medal = match rank { 1 => "🥇", 2 => "🥈", 3 => "🥉", _ => "  " };
        let dist_str = format_dist(*dist);

        // Find their DM room (if they are a DM participant).
        let dm_room_id = match uid.parse::<OwnedUserId>() {
            Ok(owned_uid) => dm_participants.get(&owned_uid).cloned(),
            Err(_)        => None,
        };

        let lang = ctx.state.lock().await
            .user_langs.get(uid.as_str()).cloned()
            .unwrap_or_else(|| "en".to_owned());

        // Geocode the player's guess in their language + build an interactive map link.
        let (pr, pg, pb, _) = crate::mapimage::PLAYER_COLORS[rank_0 % crate::mapimage::PLAYER_COLORS.len()];
        let guess_map_url = build_guess_map_url(guess.lat, guess.lon, actual_lat, actual_lon, pr, pg, pb, &lang);
        let guess_label = crate::geocode::reverse_geocode(guess.lat, guess.lon, &lang)
            .await
            .unwrap_or_else(|| format!("{:.2}, {:.2}", guess.lat, guess.lon));

        // Geocode the actual location in their language.
        let actual_label = crate::geocode::reverse_geocode(actual_lat, actual_lon, &lang)
            .await
            .unwrap_or_else(|| "Map".to_owned());

        let fb_text = format!(
            "{medal} Rank #{rank} of {}\n\
             [{guess_label}]({guess_map_url}) · {dist_str} · {score} pts\n\
             📍 Actual: [{actual_label}]({})",
            scored.len(),
            maps_url,
        );

        if let Some(dm_room_id) = dm_room_id {
            if let Some(dm_room) = client.get_room(&dm_room_id) {
                dm_room.send(format::mentionify(&fb_text)).await.ok();

                // Map image: blue dot = guess, red dot = actual, orange line.
                let (g_lat, g_lon, dist_val) = (guess.lat, guess.lon, *dist);
                if let Ok(Some(png)) = tokio::task::spawn_blocking(move || {
                    crate::mapimage::render_guess_map(g_lat, g_lon, actual_lat, actual_lon, dist_val)
                })
                .await
                {
                    let map_mime: mime::Mime = "image/png".parse().unwrap();
                    if let Ok(resp) = client.media().upload(&map_mime, png, None).await {
                        let label = format!("📍 {} away", format_dist(dist_val));
                        let img = ImageMessageEventContent::plain(label, resp.content_uri);
                        dm_room
                            .send(RoomMessageEventContent::new(MessageType::Image(img)))
                            .await
                            .ok();
                    }
                }
            }
        }
    }

    // Also DM players who did NOT submit a guess.
    for (uid, dm_room_id) in dm_participants {
        let already_got_fb = scored.iter().any(|(u, _, _, _)| {
            u.parse::<OwnedUserId>().ok().as_ref() == Some(uid)
        });
        if !already_got_fb {
            if let Some(dm_room) = client.get_room(dm_room_id) {
                let lang = ctx.state.lock().await
                    .user_langs.get(uid.as_str()).cloned()
                    .unwrap_or_else(|| "en".to_owned());
                let actual_label = crate::geocode::reverse_geocode(actual_lat, actual_lon, &lang)
                    .await
                    .unwrap_or_else(|| "Map".to_owned());
                dm_room.send(format::mentionify(&format!(
                    "⏰ Time's up! No guess submitted.\n\
                     📍 Actual: [{actual_label}]({})",
                    maps_url,
                )))
                .await
                .ok();
            }
        }
    }
}

async fn post_round_summary_free_guess(
    ctx:             &BotContext,
    client:          &Client,
    _room:           &Room,
    round_id:        i64,
    scores:          &HashMap<String, i64>,
    dm_participants: &HashMap<OwnedUserId, OwnedRoomId>,
) {
    if scores.is_empty() {
        if let Some(r) = client.get_room(&ctx.room_id) {
            r.send(format::mentionify("🌍 Round over · nobody played.")).await.ok();
        }
        return;
    }

    // ── Round results (this round only) ──────────────────────────────────────
    let n_guesses = ctx.config.schedule.guesses_per_round as usize;
    let max_pts  = 5000i64 * n_guesses as i64;

    // Fetch per-user stats for this round from DB.
    let round_stats = ctx.db.round_stats(round_id).await.unwrap_or_default();

    const BAR_W: usize = 10;

    let mut lines = vec![
        format!("🌍 **Round over!** {} guess(es) · max {} pts", n_guesses, max_pts),
        String::new(),
    ];

    // Sort by total score desc. Use DB stats if available, fall back to in-memory scores.
    let mut ranking: Vec<(&str, i64)> = scores.iter().map(|(u, &s)| (u.as_str(), s)).collect();
    ranking.sort_by(|a, b| b.1.cmp(&a.1));

    for (i, (uid, score)) in ranking.iter().enumerate() {
        let medal = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "  " };

        // Bar: fraction of max possible score.
        let filled = ((*score as f64 / max_pts as f64) * BAR_W as f64).round() as usize;
        let bar    = format!("{}{}", "█".repeat(filled.min(BAR_W)), "░".repeat(BAR_W - filled.min(BAR_W)));

        let pts_per_guess = if n_guesses > 0 { score / n_guesses as i64 } else { 0 };

        // Distance stats from DB if available for this user.
        let (avg_dist, best_dist) = round_stats
            .iter()
            .find(|e| e.user_id == *uid)
            .map(|e| (format_dist(e.avg_distance_km), format_dist(e.best_distance_km)))
            .unwrap_or_else(|| ("n/a".to_owned(), "n/a".to_owned()));

        lines.push(format!("{medal} {:>2}. {} : {} pts/guess", i + 1, uid, pts_per_guess));
        lines.push(format!("      {bar}  ⌀ {}  🏅 {}", avg_dist, best_dist));
    }

    let round_text = lines.join("\n");

    if let Some(r) = client.get_room(&ctx.room_id) {
        r.send(format::mentionify(&round_text)).await.ok();
    }
    for dm_room_id in dm_participants.values() {
        if let Some(dm_room) = client.get_room(dm_room_id) {
            dm_room.send(format::mentionify(&round_text)).await.ok();
        }
    }

    // ── All-time leaderboard ──────────────────────────────────────────────────
    if let Some(lb_text) = crate::commands::build_alltime_leaderboard(ctx).await {
        if let Some(r) = client.get_room(&ctx.room_id) {
            r.send(format::mentionify(&lb_text)).await.ok();
        }
    }
}

// ── Geocoding + scoring ───────────────────────────────────────────────────────

/// Nominatim (OpenStreetMap) geocoding.
///
/// Accepts:
/// - Raw "lat,lon" coordinates (returned directly).
/// - Any free-form query: city, country, or full postal address such as
///   "Unter den Linden 1, 10117 Berlin, Germany".
///   Nominatim handles structured addresses via its free-form search endpoint.
pub async fn geocode(query: &str) -> Option<(f64, f64)> {
    // Fast path: raw lat,lon.
    if let Some((a, b)) = query.split_once(',') {
        let a2 = a.trim();
        let b2 = b.trim();
        // Only treat as coordinates when both parts look like plain numbers
        // (i.e. no letters). This avoids mis-parsing "Berlin, Germany".
        if !a2.chars().any(|c| c.is_alphabetic()) && !b2.chars().any(|c| c.is_alphabetic()) {
            if let (Ok(lat), Ok(lon)) = (a2.parse::<f64>(), b2.parse::<f64>()) {
                if lat.abs() <= 90.0 && lon.abs() <= 180.0 {
                    return Some((lat, lon));
                }
            }
        }
    }

    let http = reqwest::Client::builder()
        .user_agent("geoguessr-bot/0.1 (matrix bot; contact: geoguessr-bot)")
        .build()
        .ok()?;

    #[derive(serde::Deserialize)]
    struct NominatimResult {
        lat: String,
        lon: String,
    }

    // Percent-encode at the byte level (correct for UTF-8 multibyte chars).
    let encoded: String = query.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
        | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        b' ' => '+'.to_string(),
        _    => format!("%{:02X}", b),
    }).collect();

    let url = format!(
        "https://nominatim.openstreetmap.org/search?q={encoded}&format=json&limit=1&addressdetails=1"
    );

    let results: Vec<NominatimResult> = http
        .get(&url)
        .send()
        .await
        .ok()?
        .json::<Vec<NominatimResult>>()
        .await
        .ok()?;

    let first = results.into_iter().next()?;
    let lat: f64 = first.lat.parse().ok()?;
    let lon: f64 = first.lon.parse().ok()?;
    Some((lat, lon))
}

/// Haversine great-circle distance in kilometres.
fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6371.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    R * 2.0 * a.sqrt().asin()
}

/// GeoGuessr-style score: 5000 × e^(−distance_km / 2000).
/// 0 km → 5000 pts, 2000 km → ~1839 pts, 5000 km → ~286 pts.
fn distance_score(distance_km: f64) -> i64 {
    (5000.0 * (-distance_km / 2000.0).exp()).round() as i64
}

pub fn format_dist(dist_km: f64) -> String {
    if dist_km < 1.0 {
        format!("{:.0} m", dist_km * 1000.0)
    } else {
        format!("{:.0} km", dist_km)
    }
}

// ── DM helpers ────────────────────────────────────────────────────────────────

/// Return an existing DM room with `user_id`, or create a new one.
pub async fn get_or_create_dm(
    client:  &Client,
    user_id: &OwnedUserId,
) -> anyhow::Result<OwnedRoomId> {
    if let Some(room) = client.get_dm_room(user_id) {
        return Ok(room.room_id().to_owned());
    }
    let room = client.create_dm(user_id).await
        .with_context(|| format!("create_dm with {user_id}"))?;
    Ok(room.room_id().to_owned())
}

/// Called immediately when a user reacts to the join-phase message.
///
/// Opens (or reuses) a 1-to-1 DM room and sends a confirmation that the game
/// is starting soon. The room is registered in `ctx.dm_rooms` straight away so
/// any message the user sends during the wait window is already routable.
pub async fn open_join_dm(
    ctx:           &BotContext,
    client:        &Client,
    user_id:       &OwnedUserId,
    reminder_secs: u64,
) {
    match get_or_create_dm(client, user_id).await {
        Ok(dm_room_id) => {
            let is_existing = ctx.dm_rooms.lock().await.contains_key(&dm_room_id);

            // Register right away — DM messages during the wait window are routed here.
            ctx.dm_rooms.lock().await.insert(dm_room_id.clone(), user_id.clone());

            if let Some(dm_room) = client.get_room(&dm_room_id) {
                let eta = if reminder_secs > 0 {
                    format!("in ~{}", format_duration(reminder_secs))
                } else {
                    "very soon".to_owned()
                };

                let msg = if is_existing {
                    format!("🌍 Game starts {eta} · photo coming here.")
                } else {
                    format!(
                        "🌍 You're in! Game starts {eta}.\n\
                         I'll send the photo here · type city, country, address, or lat,lon."
                    )
                };
                dm_room.send(format::mentionify(&msg)).await.ok();
            }
        }
        Err(e) => warn!("Could not open DM with {user_id} on join reaction: {e}"),
    }
}

/// Query the server for all reactions on `join_event_id` with `join_emoji`
/// and add matching users to `participants` (excluding the bot).
/// Flags the bot posts as reactions on the join message (the most common ones).
/// Any flag recognised by `flag_to_lang` counts as a join, even unlisted ones.
const JOIN_REACTION_FLAGS: &[&str] = &["🇬🇧", "🇩🇪", "🇺🇦", "🇫🇷"];

/// Map a flag emoji to a BCP-47 language code.
/// Returns `None` for unrecognised flags.
pub fn flag_to_lang(flag: &str) -> Option<&'static str> {
    match flag {
        "🇬🇧" | "🇺🇸" | "🇦🇺" | "🇨🇦" | "🇮🇪" | "🇳🇿" => Some("en"),
        "🇩🇪" | "🇦🇹"                                    => Some("de"),
        "🇺🇦"                                            => Some("uk"),
        "🇫🇷" | "🇧🇪" | "🇲🇨"                           => Some("fr"),
        "🇪🇸" | "🇲🇽" | "🇦🇷" | "🇨🇴" | "🇨🇱"          => Some("es"),
        "🇷🇺" | "🇧🇾"                                    => Some("ru"),
        "🇮🇹" | "🇸🇲"                                    => Some("it"),
        "🇵🇱"                                            => Some("pl"),
        "🇳🇱"                                            => Some("nl"),
        "🇵🇹" | "🇧🇷"                                    => Some("pt"),
        "🇯🇵"                                            => Some("ja"),
        "🇨🇳" | "🇹🇼" | "🇭🇰"                           => Some("zh"),
        "🇸🇦" | "🇦🇪" | "🇪🇬" | "🇲🇦" | "🇩🇿"          => Some("ar"),
        "🇹🇷"                                            => Some("tr"),
        "🇸🇪"                                            => Some("sv"),
        "🇫🇮"                                            => Some("fi"),
        "🇩🇰"                                            => Some("da"),
        "🇨🇿"                                            => Some("cs"),
        "🇭🇺"                                            => Some("hu"),
        "🇷🇴"                                            => Some("ro"),
        "🇬🇷"                                            => Some("el"),
        "🇮🇱"                                            => Some("he"),
        "🇰🇷"                                            => Some("ko"),
        "🇹🇭"                                            => Some("th"),
        "🇻🇳"                                            => Some("vi"),
        "🇮🇩" | "🇲🇾"                                   => Some("id"),
        _                                                => None,
    }
}

/// Map a BCP-47 language code string to the canonical code we store.
/// Accepts lowercase codes; returns `None` for unsupported codes.
pub fn text_code_to_lang(code: &str) -> Option<&'static str> {
    match code {
        "en" => Some("en"), "de" => Some("de"), "uk" => Some("uk"),
        "fr" => Some("fr"), "es" => Some("es"), "ru" => Some("ru"),
        "it" => Some("it"), "pl" => Some("pl"), "nl" => Some("nl"),
        "pt" => Some("pt"), "ja" => Some("ja"), "zh" => Some("zh"),
        "ar" => Some("ar"), "tr" => Some("tr"), "sv" => Some("sv"),
        "fi" => Some("fi"), "da" => Some("da"), "cs" => Some("cs"),
        "hu" => Some("hu"), "ro" => Some("ro"), "el" => Some("el"),
        "he" => Some("he"), "ko" => Some("ko"), "th" => Some("th"),
        "vi" => Some("vi"), "id" => Some("id"),
        _ => None,
    }
}

/// Human-readable name for a BCP-47 language code.
pub fn lang_label(code: &str) -> &'static str {
    match code {
        "en" => "English",  "de" => "German",     "uk" => "Ukrainian",
        "fr" => "French",   "es" => "Spanish",    "ru" => "Russian",
        "it" => "Italian",  "pl" => "Polish",     "nl" => "Dutch",
        "pt" => "Portuguese","ja" => "Japanese",  "zh" => "Chinese",
        "ar" => "Arabic",   "tr" => "Turkish",    "sv" => "Swedish",
        "fi" => "Finnish",  "da" => "Danish",     "cs" => "Czech",
        "hu" => "Hungarian","ro" => "Romanian",   "el" => "Greek",
        "he" => "Hebrew",   "ko" => "Korean",     "th" => "Thai",
        "vi" => "Vietnamese","id" => "Indonesian",
        _ => "unknown",
    }
}

async fn reconcile_join_reactions(
    client:        &Client,
    room:          &Room,
    join_event_id: &OwnedEventId,
    bot_user_id:   Option<&matrix_sdk::ruma::UserId>,
    participants:  &mut HashSet<OwnedUserId>,
) {
    use matrix_sdk::ruma::{
        api::client::relations::get_relating_events_with_rel_type_and_event_type::v1 as api,
        events::{AnyMessageLikeEvent, TimelineEventType, relation::RelationType},
    };

    let mut from: Option<String> = None;
    loop {
        let mut req = api::Request::new(
            room.room_id().to_owned(),
            join_event_id.clone(),
            RelationType::Annotation,
            TimelineEventType::from("m.reaction"),
        );
        req.from = from.clone();
        match client.send(req).await {
            Ok(resp) => {
                for raw in &resp.chunk {
                    let Ok(AnyMessageLikeEvent::Reaction(ev)) = raw.deserialize() else { continue };
                    let Some(orig) = ev.as_original() else { continue };
                    if bot_user_id.map(|b| b == orig.sender).unwrap_or(false) { continue; }
                    if flag_to_lang(&orig.content.relates_to.key).is_some() {
                        participants.insert(orig.sender.clone());
                    }
                }
                match resp.next_batch {
                    Some(t) => from = Some(t),
                    None    => break,
                }
            }
            Err(e) => {
                warn!("reconcile_join_reactions failed: {e}");
                break;
            }
        }
    }
}

// ── Reveal message — multiple choice ─────────────────────────────────────────

async fn post_reveal(
    ctx:           &BotContext,
    client:        &Client,
    _room:         &Room,
    img:           &GeoImage,
    choices:       &[String],
    correct_index: u8,
    answers:       &HashMap<String, AnswerRecord>,
    n_correct:     usize,
) {
    let location_str = match &img.city {
        Some(city) => format!("{}, {}", city, img.country),
        None       => img.country.clone(),
    };

    let correct_emoji = CHOICE_EMOJIS[correct_index as usize];
    let correct_name  = &choices[correct_index as usize];

    let mut lines = vec![
        format!("📍 **{}** {} {}", location_str, correct_emoji, correct_name),
        String::new(),
    ];

    if answers.is_empty() {
        lines.push("Nobody answered.".to_owned());
    } else {
        let n_total = answers.len();
        lines.push(format!("{n_correct}/{n_total} correct"));
        lines.push(String::new());

        let mut correct_users: Vec<(&str, &AnswerRecord)> = answers
            .iter()
            .filter(|(_, r)| r.choice == correct_index)
            .map(|(id, r)| (id.as_str(), r))
            .collect();
        correct_users.sort_by_key(|(_, r)| r.submitted_at);

        for (uid, _rec) in &correct_users {
            lines.push(format!("✅ {}", uid));
        }

        let wrong: Vec<&str> = answers
            .iter()
            .filter(|(_, r)| r.choice != correct_index)
            .map(|(id, _)| id.as_str())
            .collect();
        if !wrong.is_empty() {
            lines.push(String::new());
            for uid in wrong {
                lines.push(format!("❌ {}", uid));
            }
        }
    }

    let text = lines.join("\n");
    if let Some(r) = client.get_room(&ctx.room_id) {
        r.send(format::mentionify(&text)).await.ok();
    }
}

// ── Round summary — multiple choice ──────────────────────────────────────────

async fn post_round_summary(
    ctx:           &BotContext,
    client:        &Client,
    _room:         &Room,
    _round_id:     i64,
    round_results: &HashMap<String, Vec<bool>>,
) {
    if round_results.is_empty() {
        if let Some(r) = client.get_room(&ctx.room_id) {
            r.send(format::mentionify("🌍 Round over · nobody played.")).await.ok();
        }
        return;
    }

    let mut scores: Vec<(&str, usize, usize)> = round_results
        .iter()
        .map(|(uid, results)| {
            let correct = results.iter().filter(|&&c| c).count();
            let total   = results.len();
            (uid.as_str(), correct, total)
        })
        .collect();
    scores.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));

    let n_guesses = ctx.config.schedule.guesses_per_round as usize;
    let mut lines = vec![format!("🌍 **Round over!** {} guess(es):", n_guesses)];
    lines.push(String::new());

    for (i, (uid, correct, total)) in scores.iter().enumerate() {
        let medal = match i { 0 => "🥇", 1 => "🥈", 2 => "🥉", _ => "  " };
        lines.push(format!("{medal} {} : {}/{}", uid, correct, total));
    }

    let text = lines.join("\n");
    if let Some(r) = client.get_room(&ctx.room_id) {
        r.send(format::mentionify(&text)).await.ok();
    }
}

// ── Image upload ──────────────────────────────────────────────────────────────

/// Upload the primary image and all extra images.
/// Returns a vec of `(mxc_uri, mime)` pairs — primary first.
/// Extra images that fail to upload are silently skipped (logged as warnings).
async fn upload_all_images(
    client: &Client,
    img:    &GeoImage,
) -> anyhow::Result<Vec<(matrix_sdk::ruma::OwnedMxcUri, mime::Mime)>> {
    let primary = upload_image(client, img).await?;
    let mut results = vec![primary];

    for url in &img.extra_image_urls {
        match upload_http_url(client, url).await {
            Ok(r)  => results.push(r),
            Err(e) => warn!("GeoGuessr: failed to upload extra image: {e}"),
        }
    }
    Ok(results)
}

/// Download an image from an HTTP(S) URL and upload it to the Matrix media store.
async fn upload_http_url(
    client: &Client,
    url:    &str,
) -> anyhow::Result<(matrix_sdk::ruma::OwnedMxcUri, mime::Mime)> {
    let resp = reqwest::Client::builder()
        .user_agent("geoguessr-bot/0.1")
        .build()?
        .get(url)
        .send()
        .await?;
    let mime_str = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .split(';')
        .next()
        .unwrap_or("image/jpeg")
        .trim()
        .to_owned();
    let mime: mime::Mime = mime_str.parse().unwrap_or(mime::IMAGE_JPEG);
    let data = resp.bytes().await?.to_vec();
    let response = client.media().upload(&mime, data, None).await?;
    Ok((response.content_uri, mime))
}

async fn upload_image(
    client: &Client,
    img:    &GeoImage,
) -> anyhow::Result<(matrix_sdk::ruma::OwnedMxcUri, mime::Mime)> {
    if img.image_url.starts_with('/') || img.image_url.starts_with("file://") {
        let path = img.image_url.trim_start_matches("file://");
        let data = tokio::fs::read(path).await?;
        let mime = detect_mime(&data);
        let response = client.media().upload(&mime, data, None).await?;
        Ok((response.content_uri, mime))
    } else {
        upload_http_url(client, &img.image_url).await
    }
}

fn detect_mime(data: &[u8]) -> mime::Mime {
    if data.starts_with(b"\x89PNG")     { mime::IMAGE_PNG }
    else if data.starts_with(b"\xff\xd8\xff") { mime::IMAGE_JPEG }
    else                                { mime::IMAGE_JPEG }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn build_question_text(
    guess_num: u32,
    n_total:   u32,
    choices:   &[String],
    remaining: u64,
    total:     u64,
) -> String {
    let bar      = time_bar(remaining, total);
    let time_str = format_duration(remaining);
    let mut lines = vec![
        format!("🌍 Guess {guess_num}/{n_total} | ⏳ {time_str}  {bar}"),
        String::new(),
    ];
    for (i, choice) in choices.iter().enumerate() {
        lines.push(format!("{}  {}", CHOICE_EMOJIS[i], choice));
    }
    lines.join("\n")
}

fn time_bar(remaining: u64, total: u64) -> String {
    const W: usize = 10;
    let denom  = total.max(1);
    let filled = ((remaining.min(denom) * W as u64) / denom) as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(W - filled))
}

/// Format a duration in seconds as a human-readable string.
/// Examples: 21600 → "6h", 5400 → "1h 30m", 90 → "1m 30s", 45 → "45s".
pub fn format_duration(secs: u64) -> String {
    if secs >= 3600 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 { format!("{h}h") } else { format!("{h}h {m}m") }
    } else if secs >= 60 {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 { format!("{m}m") } else { format!("{m}m {s}s") }
    } else {
        format!("{secs}s")
    }
}

/// Build a geojson.io URL that shows:
///   • the player's guess marker in their assigned colour
///   • the actual location marker in dark/black
///   • a coloured line connecting the two
///
/// Everything is encoded in the URL fragment so no server state is needed.
/// The base map on geojson.io uses OSM-based tiles.
fn build_guess_map_url(
    guess_lat:  f64, guess_lon:  f64,
    actual_lat: f64, actual_lon: f64,
    r: u8, g: u8, b: u8,
    lang: &str,
) -> String {
    format!(
        "https://polynomialdivision.github.io/geo-picker/reveal.html\
         ?lang={lang}&alat={actual_lat:.4}&alon={actual_lon:.4}\
         &glat={guess_lat:.4}&glon={guess_lon:.4}&color={r:02X}{g:02X}{b:02X}"
    )
}

/// Build a reveal.html URL showing every player's guess and the actual location.
fn build_all_guesses_map_url(
    entries:    &[(&str, f64, f64, u8, u8, u8)],  // (uid, lat, lon, r, g, b)
    actual_lat: f64,
    actual_lon: f64,
    names:      &HashMap<String, String>,
) -> String {
    let mut url = format!(
        "https://polynomialdivision.github.io/geo-picker/reveal.html\
         ?lang=en&alat={actual_lat:.4}&alon={actual_lon:.4}"
    );
    for (uid, lat, lon, r, g, b) in entries {
        let name = url_encode_component(display_name(names, uid));
        url.push_str(&format!("&g={name}|{lat:.4}|{lon:.4}|{r:02X}{g:02X}{b:02X}"));
    }
    url
}

/// Percent-encode using the RFC 3986 unreserved-character set
/// (equivalent to JavaScript's `encodeURIComponent`).
fn url_encode_component(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => { let _ = write!(out, "%{b:02X}"); }
        }
    }
    out
}

fn display_name<'a>(names: &'a HashMap<String, String>, uid: &'a str) -> &'a str {
    names.get(uid).map(|s| s.as_str()).unwrap_or_else(|| {
        uid.split(':').next().unwrap_or("").trim_start_matches('@')
    })
}

async fn fetch_names(room: &Room, user_ids: &[&str]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for &uid_str in user_ids {
        if let Ok(uid) = matrix_sdk::ruma::OwnedUserId::try_from(uid_str) {
            if let Ok(Some(member)) = room.get_member(&uid).await {
                let name = member
                    .display_name()
                    .unwrap_or_else(|| member.user_id().localpart())
                    .to_owned();
                map.insert(uid_str.to_owned(), name);
            }
        }
    }
    map
}

async fn reconcile_reactions(
    client:     &Client,
    room:       &Room,
    q_event_id: &OwnedEventId,
    answers:    &mut HashMap<String, AnswerRecord>,
) {
    use matrix_sdk::ruma::{
        api::client::relations::get_relating_events_with_rel_type_and_event_type::v1 as api,
        events::{AnyMessageLikeEvent, TimelineEventType, relation::RelationType},
    };

    let mut server_answers: HashMap<String, u8> = HashMap::new();
    let mut from: Option<String> = None;
    let mut ok = false;

    loop {
        let mut req = api::Request::new(
            room.room_id().to_owned(),
            q_event_id.clone(),
            RelationType::Annotation,
            TimelineEventType::from("m.reaction"),
        );
        req.from = from.clone();
        match client.send(req).await {
            Ok(resp) => {
                ok = true;
                for raw in &resp.chunk {
                    let Ok(AnyMessageLikeEvent::Reaction(ev)) = raw.deserialize() else { continue };
                    let Some(orig) = ev.as_original() else { continue };
                    if client.user_id().map(|id| id == orig.sender).unwrap_or(false) { continue; }
                    let choice = match orig.content.relates_to.key.as_str() {
                        "🇦" => 0u8, "🇧" => 1, "🇨" => 2, "🇩" => 3, _ => continue,
                    };
                    server_answers.entry(orig.sender.as_str().to_owned()).or_insert(choice);
                }
                match resp.next_batch {
                    Some(t) => from = Some(t),
                    None    => break,
                }
            }
            Err(e) => {
                warn!("GeoGuessr: reaction reconciliation failed: {e}");
                break;
            }
        }
    }

    if !ok { return; }

    let now = chrono::Utc::now();
    for (uid, choice) in server_answers {
        answers.entry(uid).or_insert(AnswerRecord {
            choice,
            source:         "reconciled",
            submitted_at:   now,
            changed_answer: false,
        });
    }
}

// ── Prefetch ──────────────────────────────────────────────────────────────────

pub async fn prefetch_if_needed(ctx: &BotContext, target: usize) {
    let current = ctx.state.lock().await.cached_guesses.len();
    let needed  = target.saturating_sub(current);
    if needed == 0 { return; }

    info!("GeoGuessr: prefetching {needed} images");

    let sources = &ctx.config.sources.enabled;

    // Collect existing coordinates and sequence IDs so each fetch in this
    // batch avoids duplicating locations already in the cache or fetched
    // earlier in the same batch.
    let (mut existing_coords, mut existing_seqs): (Vec<(f64, f64)>, Vec<Option<String>>) = {
        let st = ctx.state.lock().await;
        st.cached_guesses.iter()
            .map(|img| {
                let coord = img.lat.zip(img.lon);
                (coord, img.sequence.clone())
            })
            .filter_map(|(coord, seq)| coord.map(|c| (c, seq)))
            .unzip()
    };

    for _ in 0..needed {
        let source = {
            let mut rng = rand::thread_rng();
            sources.choose(&mut rng).map(|s| s.as_str()).unwrap_or("wikimedia").to_owned()
        };

        let n_photos = ctx.config.schedule.photos_per_location;
        let result = match source.as_str() {
            "mapillary" => {
                crate::sources::mapillary::fetch(
                    &ctx.config.sources.mapillary,
                    n_photos,
                    &existing_coords,
                    &existing_seqs,
                ).await
            }
            "local" => {
                match &ctx.config.sources.local.path {
                    Some(p) => crate::sources::local::fetch(p).await,
                    None    => {
                        warn!("GeoGuessr: local source enabled but no path configured");
                        continue;
                    }
                }
            }
            _ => crate::sources::wikimedia::fetch(
                &ctx.config.sources.wikimedia,
                n_photos,
                &existing_coords,
            ).await,
        };

        match result {
            Ok(img) => {
                // Update the local diversity lists so the next iteration in
                // this batch also respects the location we just fetched.
                if let Some(coord) = img.lat.zip(img.lon) {
                    existing_coords.push(coord);
                    existing_seqs.push(img.sequence.clone());
                }
                let mut st = ctx.state.lock().await;
                st.cached_guesses.push_back(img);
                st.save(&ctx.state_path).await.ok();
            }
            Err(e) => warn!("GeoGuessr: prefetch failed ({source}): {e}"),
        }
    }
}

// ── Resume after restart ──────────────────────────────────────────────────────

/// Called on startup when a join phase was in progress at shutdown time.
/// Waits until the game-start instant, re-reads reactions to rebuild the
/// participant list, then runs the game exactly as `start_round` would.
pub async fn resume_pending_join(ctx: BotContext, client: Client, pj: PendingJoin) {
    // Wait until game_at_utc.
    let now_utc   = chrono::Utc::now();
    let wait_secs = (pj.game_at_utc - now_utc)
        .max(chrono::Duration::zero())
        .num_seconds() as u64;
    if wait_secs > 0 {
        info!("resume_pending_join: waiting {wait_secs}s until game start");
        tokio::time::sleep(tokio::time::Duration::from_secs(wait_secs)).await;
    }

    let room = match client.get_room(&ctx.room_id) {
        Some(r) => r,
        None    => {
            warn!("resume_pending_join: room {} not joined", ctx.room_id);
            let mut st = ctx.state.lock().await;
            st.pending_join = None;
            st.save(&ctx.state_path).await.ok();
            return;
        }
    };

    // Parse the saved event ID.
    let join_event_id: OwnedEventId = match pj.event_id.parse() {
        Ok(id) => id,
        Err(e) => {
            warn!("resume_pending_join: invalid event_id: {e}");
            let mut st = ctx.state.lock().await;
            st.pending_join = None;
            st.save(&ctx.state_path).await.ok();
            return;
        }
    };

    // Re-read reactions from the join message to rebuild the participant set.
    let bot_uid = client.user_id().map(|u| u.to_owned());
    // Start from in-memory participants captured since the restart.
    let mut participants: HashSet<OwnedUserId> = {
        let mut js = ctx.join_state.lock().await;
        js.message_event_id = None;
        std::mem::take(&mut js.participants)
    };
    reconcile_join_reactions(
        &client,
        &room,
        &join_event_id,
        bot_uid.as_deref(),
        &mut participants,
    )
    .await;

    // Clear pending_join.
    {
        let mut st = ctx.state.lock().await;
        st.pending_join = None;
        st.save(&ctx.state_path).await.ok();
    }

    let n            = ctx.config.schedule.guesses_per_round as usize;
    let triggered_by = "scheduler";

    if participants.is_empty() {
        room.send(format::mentionify("😴 Nobody joined · skipping."))
            .await.ok();
        if let Some(slot) = &pj.slot {
            let tz: Tz = ctx.config.schedule.timezone.parse().unwrap_or(chrono_tz::UTC);
            let today  = chrono::Utc::now().with_timezone(&tz).date_naive();
            let mut st = ctx.state.lock().await;
            st.last_game_dates.insert(slot.clone(), today);
            st.save(&ctx.state_path).await.ok();
        }
        return;
    }

    // Open (or reuse) DMs with each participant.
    let mut dm_map: HashMap<OwnedUserId, OwnedRoomId> = HashMap::new();
    for uid in &participants {
        match get_or_create_dm(&client, uid).await {
            Ok(dm_room_id) => {
                ctx.dm_rooms.lock().await.insert(dm_room_id.clone(), uid.clone());
                dm_map.insert(uid.clone(), dm_room_id);
            }
            Err(e) => warn!("resume_pending_join: could not open DM with {uid}: {e}"),
        }
    }

    let participant_list: Vec<String> = dm_map.keys()
        .map(|u| u.to_string())
        .collect();
    let participant_ids: Vec<OwnedUserId> = dm_map.keys().cloned().collect();
    room.send(
        format::mentionify(&format!(
            "🌍 GeoGuessr starting now! {} player{}: {}",
            dm_map.len(),
            if dm_map.len() == 1 { "" } else { "s" },
            participant_list.join(", "),
        ))
        .add_mentions(Mentions::with_user_ids(participant_ids)),
    ).await.ok();

    // Pre-fetch and run images.
    prefetch_if_needed(&ctx, n + 2).await;

    let round_id = match ctx.db
        .start_round(ctx.room_id.as_str(), n as u32, triggered_by)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            error!("resume_pending_join: failed to start DB round: {e}");
            return;
        }
    };

    info!("GeoGuessr round {round_id} resumed after restart ({n} images)");

    let mut round_scores_free: HashMap<String, i64> = HashMap::new();

    for i in 0..n {
        let img = {
            let mut st = ctx.state.lock().await;
            match st.cached_guesses.pop_front() {
                Some(img) => {
                    st.save(&ctx.state_path).await.ok();
                    img
                }
                None => {
                    warn!("resume_pending_join: guess cache empty — skipping remaining");
                    break;
                }
            }
        };

        if i > 0 {
            tokio::time::sleep(tokio::time::Duration::from_secs(
                ctx.config.schedule.inter_guess_secs,
            )).await;
        }

        if let Err(e) = play_free_guess(
            &ctx, &client, &room,
            round_id, i as u32 + 1, n as u32,
            &img, &mut round_scores_free, &dm_map,
            pj.answer_timeout_secs,
        ).await {
            error!("resume_pending_join: play_free_guess error: {e}");
        }
    }

    ctx.db.finish_round(round_id).await.ok();
    ctx.db.upsert_round_scores_free_guess(round_id, &round_scores_free).await.ok();

    if let Some(slot) = &pj.slot {
        let tz: Tz = ctx.config.schedule.timezone.parse().unwrap_or(chrono_tz::UTC);
        let today  = chrono::Utc::now().with_timezone(&tz).date_naive();
        let mut st = ctx.state.lock().await;
        st.last_game_dates.insert(slot.clone(), today);
        st.save(&ctx.state_path).await.ok();
    }

    post_round_summary_free_guess(&ctx, &client, &room, round_id, &round_scores_free, &dm_map).await;

    // Clear DM mappings.
    {
        let dm_room_ids: Vec<OwnedRoomId> = dm_map.values().cloned().collect();
        let mut dm_rooms = ctx.dm_rooms.lock().await;
        for id in &dm_room_ids {
            dm_rooms.remove(id);
        }
    }
    *ctx.round_abort.lock().await = None;
}

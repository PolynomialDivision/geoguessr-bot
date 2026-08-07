//! Round and image game logic.

use std::collections::{HashMap, HashSet, VecDeque};

use chrono_tz::Tz;
use matrix_sdk::{
    ruma::{
        events::{
            reaction::ReactionEventContent,
            relation::Annotation,
            room::{
                message::{
                    ImageMessageEventContent, MessageType, ReplacementMetadata,
                    RoomMessageEventContent,
                },
                pinned_events::RoomPinnedEventsEventContent,
                ImageInfo,
            },
            Mentions,
        },
        // (ReactionEventContent and Annotation kept for join-phase reactions)
        OwnedEventId,
        OwnedMxcUri,
        OwnedUserId,
        UInt,
    },
    Client, Room,
};
use rand::seq::SliceRandom;
use tracing::{error, info, warn};

use crate::{
    countries, format,
    sources::GeoImage,
    state::{ActiveDmParticipant, ActiveRoundState, PendingJoin},
    BotContext,
};

/// Metadata about an image that has been uploaded to the Matrix media store.
struct UploadedMedia {
    uri: OwnedMxcUri,
    mime: mime::Mime,
    w: u32,
    h: u32,
    size: usize,
}

// ── Per-round overrides ───────────────────────────────────────────────────────

/// Optional overrides for a single game round, used by `!schedulegeo`.
pub struct GameOverrides {
    /// Override for how long before game time the join message fires (seconds).
    pub reminder_before_secs: Option<u64>,
    /// Override for how long players have to answer (seconds).
    pub answer_timeout_secs: Option<u64>,
    /// Override for how many guesses (locations) per round.
    pub guesses_per_round: Option<u32>,
}

// ── Free-guess answer ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct FreeGuess {
    pub text: String,
    pub lat: f64,
    pub lon: f64,
    pub submitted_at: chrono::DateTime<chrono::Utc>,
}

// ── Active game state (in-memory only) ────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ActiveGame {
    pub event_id: OwnedEventId,
    pub mode: ActiveGameMode,
}

#[derive(Clone, Debug)]
pub enum ActiveGameMode {
    FreeGuess {
        guesses: HashMap<String, FreeGuess>,
        actual_lat: f64,
        actual_lon: f64,
    },
}

impl ActiveGame {
    /// Record a guess for `user_id`.  Returns `true` if accepted, `false` if
    /// the player has already guessed and `max_guesses` > 0.
    pub fn record_free_guess(
        &mut self,
        user_id: String,
        guess: FreeGuess,
        max_guesses: u32,
    ) -> bool {
        let ActiveGameMode::FreeGuess {
            ref mut guesses, ..
        } = self.mode;
        if max_guesses > 0 && guesses.contains_key(&user_id) {
            return false;
        }
        guesses.insert(user_id, guess);
        true
    }
}

// ── Round entry point ─────────────────────────────────────────────────────────

pub async fn start_round(
    ctx: BotContext,
    client: Client,
    manual: bool,
    slot: Option<String>,
    overrides: Option<GameOverrides>,
) -> anyhow::Result<()> {
    let room = match client.get_room(&ctx.room_id) {
        Some(r) => r,
        None => {
            warn!("GeoGuessr: room {} not joined", ctx.room_id);
            return Ok(());
        }
    };

    let n = overrides
        .as_ref()
        .and_then(|o| o.guesses_per_round)
        .map(|v| v as usize)
        .unwrap_or(ctx.effective_guesses_per_round().await);
    let triggered_by = if manual { "manual" } else { "scheduler" };

    // Apply per-round overrides (from !schedulegeo).
    let reminder_before_secs_cfg = overrides
        .as_ref()
        .and_then(|o| o.reminder_before_secs)
        .unwrap_or(ctx.config.schedule.reminder_before_secs);
    let answer_timeout_secs_cfg = overrides
        .as_ref()
        .and_then(|o| o.answer_timeout_secs)
        .unwrap_or(ctx.config.schedule.answer_timeout_secs);

    // ── Join phase (scheduled only) ───────────────────────────────────────────
    // When reminder_before_secs > 0, post a "who wants to play?" message,
    // react to it, and wait for participants.
    let participants: Vec<OwnedUserId> = if !manual && reminder_before_secs_cfg > 0 {
        let reminder_secs = reminder_before_secs_cfg;
        let emoji = ctx.config.schedule.join_emoji.clone();

        // Post the join-prompt message.
        let flags_str = JOIN_REACTION_FLAGS.join(" ");
        let join_msg = format!(
            "🌍 GeoGuessr starts in {}! @room\n\
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
        let join_event = room.send(join_content).await?;
        let join_event_id = join_event.response.event_id.clone();
        set_pinned(&room, &join_event_id).await;

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
            js.join_emoji = emoji.clone();
            js.participants.clear();
        }

        // Persist the join phase so a restart can resume it.
        {
            let game_at_utc = chrono::Utc::now() + chrono::Duration::seconds(reminder_secs as i64);
            let mut st = ctx.state.lock().await;
            st.pending_join = Some(PendingJoin {
                event_id: join_event_id.to_string(),
                join_emoji: emoji.clone(),
                slot: slot.clone(),
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
            room.send(format::mentionify("😴 Nobody joined · skipping."))
                .await
                .ok();
            // Mark slot as done so we don't retry.
            if let Some(slot) = slot {
                let tz: Tz = ctx
                    .config
                    .schedule
                    .timezone
                    .parse()
                    .unwrap_or(chrono_tz::UTC);
                let today = chrono::Utc::now().with_timezone(&tz).date_naive();
                let mut st = ctx.state.lock().await;
                st.last_game_dates.insert(slot, today);
                st.save(&ctx.state_path).await.ok();
            }
            return Ok(());
        }

        let mut sorted: Vec<OwnedUserId> = participants.into_iter().collect();
        sorted.sort();

        let participant_list: Vec<String> = sorted.iter().map(|u| u.to_string()).collect();
        let participant_ids: Vec<OwnedUserId> = sorted.clone();
        let n = sorted.len();
        room.send(
            format::mentionify(&format!(
                "🌍 GeoGuessr starting now! {} player{}: {}",
                n,
                if n == 1 { "" } else { "s" },
                participant_list.join(", "),
            ))
            .add_mentions(Mentions::with_user_ids(participant_ids)),
        )
        .await
        .ok();

        sorted
    } else {
        Vec::new()
    };

    // ── Pre-fetch + pop images upfront ────────────────────────────────────────
    prefetch_if_needed(&ctx, n).await;

    let round_id = ctx
        .db
        .start_round(ctx.room_id.as_str(), n as u32, triggered_by)
        .await?;

    info!("GeoGuessr round {round_id} started ({n} images, triggered_by={triggered_by})");

    // Pop images from the cache with a final dedup gate.
    //
    // We re-read played coords here rather than relying solely on the eviction
    // pass inside prefetch_if_needed, because a concurrent background refill
    // (spawned at the end of the *previous* round) may have added a stale entry
    // between that eviction and this pop.
    let pop_db_coords = ctx.db.recent_played_coords().await.unwrap_or_default();
    let n_photos = ctx.effective_photos_per_location().await;
    let mut image_queue: VecDeque<GeoImage> = {
        let mut st = ctx.state.lock().await;
        let mut q = VecDeque::new();
        let limit = st.cached_guesses.len();
        let mut checked = 0;
        while q.len() < n && checked < limit {
            let img = st.cached_guesses.pop_front().expect("bounded by limit");
            checked += 1;
            let far_enough = img
                .lat
                .zip(img.lon)
                .map(|(la, lo)| {
                    crate::sources::min_dist_to_existing(la, lo, &pop_db_coords)
                        >= crate::sources::MIN_DISTANCE_KM
                })
                .unwrap_or(true); // no coords → can't check, accept
            let enough_photos = img.extra_image_urls.len() + 1 >= n_photos;
            if !far_enough {
                warn!(
                    "GeoGuessr: pop-time dedup: discarded {} (too close to a played location)",
                    img.country
                );
            } else if !enough_photos {
                warn!(
                    "GeoGuessr: pop-time dedup: discarded {} ({} photo(s) cached, {} required)",
                    img.country,
                    img.extra_image_urls.len() + 1,
                    n_photos,
                );
            } else {
                q.push_back(img);
            }
        }
        if q.len() < n {
            warn!(
                "GeoGuessr: round will use {} image(s) instead of {n} after pop-time dedup",
                q.len()
            );
        }
        st.save(&ctx.state_path).await.ok();
        q
    };

    if image_queue.is_empty() && n > 0 {
        abort_round_no_images(&ctx, &client, round_id, n).await;
        return Ok(());
    }

    let mut round_scores_free: HashMap<String, i64> = HashMap::new();

    let mut first_guess = true;
    while let Some(img) = image_queue.pop_front() {
        let guess_num = (n - image_queue.len()) as u32; // 1-based after pop

        // Save active round state so a restart can resume.
        {
            let mut st = ctx.state.lock().await;
            let dm_state = participants
                .iter()
                .map(|uid| {
                    (
                        uid.to_string(),
                        ActiveDmParticipant {
                            dm_room_id: String::new(),
                            prompt_event_id: None,
                            answer_acked: false,
                        },
                    )
                })
                .collect();
            st.active_round = Some(ActiveRoundState {
                round_id,
                guess_num,
                total_guesses: n as u32,
                current_image: img.clone(),
                remaining_images: image_queue.clone(),
                guess_started_at: chrono::Utc::now(),
                answer_timeout_secs: answer_timeout_secs_cfg,
                dm_participants: dm_state,
                round_scores: round_scores_free.clone(),
            });
            st.save(&ctx.state_path).await.ok();
        }

        if !first_guess {
            tokio::time::sleep(tokio::time::Duration::from_secs(
                ctx.config.schedule.inter_guess_secs,
            ))
            .await;
        }
        first_guess = false;

        play_free_guess(
            &ctx,
            &client,
            &room,
            round_id,
            guess_num,
            n as u32,
            &img,
            &mut round_scores_free,
            &participants,
            answer_timeout_secs_cfg,
        )
        .await?;
    }

    // ── Finalise round ────────────────────────────────────────────────────────
    ctx.db.finish_round(round_id).await?;
    ctx.db
        .upsert_round_scores_free_guess(round_id, &round_scores_free)
        .await?;

    // Background refill — placed here so all DB writes for this round (including
    // every start_guess call) are committed before recent_played_coords() is read
    // inside the next prefetch_if_needed.  Previously this was spawned right after
    // the pop, which created a window where the refill snapshot was missing the
    // current round's locations.
    {
        let ctx2 = ctx.clone();
        tokio::spawn(async move {
            prefetch_if_needed(&ctx2, n + 2).await;
        });
    }

    if let Some(slot) = slot {
        let tz: Tz = ctx
            .config
            .schedule
            .timezone
            .parse()
            .unwrap_or(chrono_tz::UTC);
        let today = chrono::Utc::now().with_timezone(&tz).date_naive();
        let mut st = ctx.state.lock().await;
        st.last_game_dates.insert(slot, today);
        st.save(&ctx.state_path).await.ok();
    }

    post_round_summary_free_guess(&ctx, &client, &room, round_id, &round_scores_free).await;

    {
        let mut st = ctx.state.lock().await;
        st.active_round = None;
        st.save(&ctx.state_path).await.ok();
    }

    Ok(())
}

/// Abort a round that has zero usable images: every cached candidate was
/// either too close to a previously-played location or was discarded by the
/// photos-per-location dedup gate (see the "pop-time dedup" warnings logged
/// just before this is called). Posts a message distinct from "nobody
/// played" so it's clear this was a supply-side failure, not a
/// participation outcome, and finishes/clears round state so it doesn't
/// linger as active.
async fn abort_round_no_images(ctx: &BotContext, client: &Client, round_id: i64, requested: usize) {
    error!(
        "GeoGuessr round {round_id}: aborting — 0 of {requested} requested image(s) available \
         after quality/dedup filtering; see preceding 'pop-time dedup' warnings for the reason(s)"
    );
    if let Some(r) = client.get_room(&ctx.room_id) {
        r.send(format::mentionify(
            "⚠️ Round aborted · no location with enough valid images was available. \
             An admin may need to run !prefetch or check the image source config.",
        ))
        .await
        .ok();
    }
    ctx.db.finish_round(round_id).await.ok();
    let mut st = ctx.state.lock().await;
    st.active_round = None;
    st.save(&ctx.state_path).await.ok();
}

/// Abort a single guess because its image could not be delivered to Matrix
/// (fetch, upload, or send failure). Posts a chat message distinct from a
/// normal timeout/reveal, and closes out the DB guess row so it isn't left
/// dangling in a "started" state. Called before the countdown timer, active
/// game registration, or DB event-id linkage happen, so no player-facing
/// state ever treats this guess as playable.
async fn abort_guess_no_image(ctx: &BotContext, room: &Room, guess_id: i64, guess_num: u32, n_total: u32) {
    room.send(format::mentionify(&format!(
        "⚠️ Guess {guess_num}/{n_total} skipped · failed to deliver the image to Matrix (see logs).",
    )))
    .await
    .ok();
    ctx.db.finish_guess(guess_id, 0, 0).await.ok();
}

// ── Single image — free guess ─────────────────────────────────────────────────

async fn play_free_guess(
    ctx: &BotContext,
    client: &Client,
    room: &Room,
    round_id: i64,
    guess_num: u32,
    n_total: u32,
    img: &GeoImage,
    round_scores: &mut HashMap<String, i64>,
    participants: &[OwnedUserId],
    answer_timeout_secs: u64,
) -> anyhow::Result<()> {
    let (actual_lat, actual_lon) = match (img.lat, img.lon) {
        (Some(lat), Some(lon)) => (lat, lon),
        _ => countries::COUNTRIES
            .iter()
            .find(|c| c.name == img.country)
            .map(|c| (c.lat, c.lon))
            .unwrap_or((0.0, 0.0)),
    };

    let guess_id = match ctx.db.find_guess_id(round_id, guess_num).await {
        Some(id) => id,
        None => {
            ctx.db
                .start_guess(
                    round_id,
                    guess_num,
                    &img.country,
                    &img.region,
                    img.city.as_deref(),
                    &img.source,
                    img.attribution.as_deref(),
                    &[],
                    0,
                    answer_timeout_secs,
                    Some(actual_lat),
                    Some(actual_lon),
                )
                .await?
        }
    };

    info!(
        "GeoGuessr guess {guess_num}/{n_total}: selected {} ({}, source={}, lat={:?}, lon={:?}) — \
         fetching/uploading image",
        img.city.as_deref().unwrap_or(&img.country),
        img.country,
        img.source,
        img.lat,
        img.lon,
    );

    // Upload all images once; reuse mxc_uris across main room + all DMs.
    let all_images = match upload_all_images(client, img).await {
        Ok(v) => v,
        Err(e) => {
            error!(
                "GeoGuessr guess {guess_num}/{n_total}: failed to fetch/upload image for {} \
                 (source={}, url={}): {e}",
                img.country, img.source, img.image_url,
            );
            abort_guess_no_image(ctx, room, guess_id, guess_num, n_total).await;
            return Ok(());
        }
    };

    info!(
        "GeoGuessr guess {guess_num}/{n_total}: uploaded {} image(s) to Matrix media store for {}",
        all_images.len(),
        img.country,
    );

    let n_imgs = all_images.len();

    // Post all images to the main room. The primary (reference) image must
    // be sent successfully before the round is allowed to become playable —
    // if it fails, abort the guess instead of starting a timer nobody can
    // see an image for. Extra context images are best-effort.
    for (i, media) in all_images.iter().enumerate() {
        let label = if i == 0 {
            if n_imgs == 1 {
                "📍 Reference location".to_owned()
            } else {
                format!("📍 Reference location (1/{n_imgs})")
            }
        } else {
            format!("📍 Context image ({}/{})", i + 1, n_imgs)
        };
        let send_result = room
            .send(image_content_with_info(
                label,
                media.uri.clone(),
                &media.mime,
                media.w,
                media.h,
                media.size,
            ))
            .await;
        match send_result {
            Ok(ev) => info!(
                "GeoGuessr guess {guess_num}/{n_total}: sent image {}/{n_imgs} to room \
                 (event_id={})",
                i + 1,
                ev.response.event_id,
            ),
            Err(e) if i == 0 => {
                error!(
                    "GeoGuessr guess {guess_num}/{n_total}: failed to send reference image \
                     event to room: {e}"
                );
                abort_guess_no_image(ctx, room, guess_id, guess_num, n_total).await;
                return Ok(());
            }
            Err(e) => warn!(
                "GeoGuessr guess {guess_num}/{n_total}: failed to send context image {}/{n_imgs}: {e}",
                i + 1,
            ),
        }
    }

    let total_secs = answer_timeout_secs;
    let timeout_str = format_duration(total_secs);

    // Generate per-player web tokens and post the initial links message.
    let web_token_map: HashMap<OwnedUserId, String> = if !participants.is_empty() {
        if let Some(ref public_url) = ctx.web_public_url {
            let mut tmap: HashMap<OwnedUserId, String> = HashMap::new();
            {
                let mut store = ctx.web_tokens.lock().await;
                for uid in participants {
                    let lang = ctx
                        .state
                        .lock()
                        .await
                        .user_langs
                        .get(uid.as_str())
                        .cloned()
                        .unwrap_or_else(|| "en".to_owned());
                    let token = crate::web::generate_token();
                    store.tokens.insert(
                        token.clone(),
                        crate::web::GuessToken {
                            user_id: uid.clone(),
                            round_id,
                            guess_num,
                            lang,
                        },
                    );
                    tmap.insert(uid.clone(), token);
                }

                let links_line = participants
                    .iter()
                    .map(|uid| {
                        let tok = &tmap[uid];
                        format!("[🗺️ {}]({}/g/{})", uid.localpart(), public_url, tok)
                    })
                    .collect::<Vec<_>>()
                    .join("  ·  ");

                if let Ok(ev) = room.send(format::mentionify(&links_line)).await {
                    set_pinned(&room, &ev.response.event_id).await;
                    store.sessions.insert(
                        (round_id, guess_num),
                        crate::web::GuessSession {
                            links_event_id: ev.response.event_id.clone(),
                            participants: participants.to_vec(),
                        },
                    );
                }
            }
            tmap
        } else {
            // No web server configured: post a geo-picker browsing link per participant
            // so they can see the map in their own language and use !guess to submit.
            let st = ctx.state.lock().await;
            let line = participants
                .iter()
                .map(|uid| {
                    let lang = st.user_langs.get(uid.as_str())
                        .cloned()
                        .unwrap_or_else(|| "en".to_owned());
                    format!(
                        "[🗺️ {}](https://polynomialdivision.github.io/geo-picker/?lang={})",
                        uid.localpart(), lang
                    )
                })
                .collect::<Vec<_>>()
                .join("  ·  ");
            drop(st);
            room.send(format::mentionify(&line)).await.ok();
            HashMap::new()
        }
    } else {
        HashMap::new()
    };

    // Countdown message: web mode shows just the timer (links message handles the rest),
    // otherwise show the !guess hint.
    let q_event = if !web_token_map.is_empty() {
        room.send(format::mentionify(&format!(
            "🌍 Guess {guess_num}/{n_total} | ⏳ {timeout_str}",
        )))
        .await?
    } else {
        room.send(format::mentionify(&format!(
            "🌍 Guess {guess_num}/{n_total} | ⏳ {timeout_str}\n\
             📍 Where is this? Type: **!guess** city, country, or lat,lon",
        )))
        .await?
    };
    let q_event_id = q_event.response.event_id.clone();

    ctx.db
        .set_guess_event_id(guess_id, q_event_id.as_str())
        .await
        .ok();

    // Register active game.
    {
        let mut ag = ctx.active_game.lock().await;
        *ag = Some(ActiveGame {
            event_id: q_event_id.clone(),
            mode: ActiveGameMode::FreeGuess {
                guesses: HashMap::new(),
                actual_lat,
                actual_lon,
            },
        });
    }

    // Smooth countdown in the main room — edits the prompt message with a time bar.
    // DMs are not updated (avoids spamming participants).
    // Early-exit: after each tick, check if every DM participant has already submitted.
    let edit_interval = (total_secs / 20).clamp(15, 900);
    let mut remaining = total_secs;
    let mut all_in = false;

    loop {
        let sleep_secs = remaining.min(edit_interval);
        tokio::time::sleep(tokio::time::Duration::from_secs(sleep_secs)).await;
        remaining -= sleep_secs;

        // Early-exit check: all participants have submitted a guess.
        if !participants.is_empty() {
            let ag = ctx.active_game.lock().await;
            if let Some(ActiveGame {
                mode: ActiveGameMode::FreeGuess { ref guesses, .. },
                ..
            }) = *ag
            {
                if participants
                    .iter()
                    .all(|uid| guesses.contains_key(uid.as_str()))
                {
                    all_in = true;
                }
            }
        }

        if all_in || remaining == 0 {
            break;
        }

        let bar = time_bar(remaining, total_secs);
        let time_str = format_duration(remaining);
        let edit_msg = if !web_token_map.is_empty() {
            format::mentionify(&format!(
                "🌍 Guess {guess_num}/{n_total} | ⏳ {time_str}  {bar}",
            ))
        } else {
            format::mentionify(&format!(
                "🌍 Guess {guess_num}/{n_total} | ⏳ {time_str}  {bar}\n\
                 📍 Where is this? Type: **!guess** city, country, or lat,lon",
            ))
        };
        if let Some(r) = client.get_room(&ctx.room_id) {
            let edit =
                edit_msg.make_replacement(ReplacementMetadata::new(q_event_id.clone(), None));
            r.send(edit).await.ok();
        }
    }

    if all_in {
        let final_msg = format!("🌍 Guess {guess_num}/{n_total} | ⚡ All guesses in!");
        if let Some(r) = client.get_room(&ctx.room_id) {
            let edit = RoomMessageEventContent::text_plain(&final_msg)
                .make_replacement(ReplacementMetadata::new(q_event_id.clone(), None));
            r.send(edit).await.ok();
        }
    }

    // Collect guesses.
    let guesses = {
        let mut ag = ctx.active_game.lock().await;
        match ag.take().map(|g| g.mode) {
            Some(ActiveGameMode::FreeGuess { guesses, .. }) => guesses,
            _ => HashMap::new(),
        }
    };

    // Clear web tokens for this guess so the links expire immediately.
    if !web_token_map.is_empty() {
        let mut store = ctx.web_tokens.lock().await;
        store
            .tokens
            .retain(|_, t| !(t.round_id == round_id && t.guess_num == guess_num));
        store.sessions.remove(&(round_id, guess_num));
    }

    // Score.
    let half_life = ctx.config.schedule.score_half_life_km;
    let mut scored: Vec<(String, FreeGuess, f64, i64)> = guesses
        .into_iter()
        .map(|(uid, guess)| {
            let dist = haversine_km(guess.lat, guess.lon, actual_lat, actual_lon);
            let score = distance_score(dist, half_life);
            (uid, guess, dist, score)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.3.cmp(&a.3)
            .then(a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.0.cmp(&b.0))
    });

    let n_answers = scored.len();

    let db_rows: Vec<(String, String, f64, f64, f64, i64)> = scored
        .iter()
        .map(|(uid, g, dist, score)| (uid.clone(), g.text.clone(), g.lat, g.lon, *dist, *score))
        .collect();
    ctx.db
        .record_free_guess_answers(guess_id, round_id, db_rows)
        .await?;
    ctx.db.finish_guess(guess_id, n_answers, 0).await?;

    for (uid, _, _, score) in &scored {
        *round_scores.entry(uid.clone()).or_insert(0) += score;
    }

    let user_ids: Vec<&str> = scored.iter().map(|(uid, _, _, _)| uid.as_str()).collect();
    let names = fetch_names(room, &user_ids).await;
    let raw_avatars = crate::avatar::fetch_player_avatars(room, &user_ids).await;
    // Build data: URLs from the already-downloaded avatar bytes so reveal.html
    // can show real photos without needing authenticated media access.
    let avatar_mxcs: HashMap<String, String> = raw_avatars
        .iter()
        .filter_map(|(uid, bytes)| {
            crate::avatar::avatar_bytes_to_data_url(bytes).map(|url| (uid.clone(), url))
        })
        .collect();
    post_reveal_free_guess(
        ctx,
        client,
        img,
        actual_lat,
        actual_lon,
        &scored,
        &names,
        &raw_avatars,
        &avatar_mxcs,
    )
    .await;

    Ok(())
}

async fn post_reveal_free_guess(
    ctx: &BotContext,
    client: &Client,
    img: &GeoImage,
    actual_lat: f64,
    actual_lon: f64,
    scored: &[(String, FreeGuess, f64, i64)],
    names: &HashMap<String, String>,
    raw_avatars: &HashMap<String, Vec<u8>>,
    avatar_mxcs: &HashMap<String, String>,
) {
    let location_str = match &img.city {
        Some(city) => format!("{}, {}", city, img.country),
        None => img.country.clone(),
    };

    let maps_url = format!(
        "https://www.openstreetmap.org/?mlat={:.4}&mlon={:.4}#map=8/{:.4}/{:.4}",
        actual_lat, actual_lon, actual_lat, actual_lon,
    );

    // ── Reverse-geocode each guess for human-readable labels ─────────────────
    // overview_entries: (uid, lat, lon, r, g, b, score, dist_km, mxc_url)
    let mut guess_labels: Vec<String> = Vec::with_capacity(scored.len());
    let mut overview_entries: Vec<(&str, f64, f64, u8, u8, u8, i64, f64, String)> =
        Vec::with_capacity(scored.len());
    for (i, (uid, guess, dist, score)) in scored.iter().enumerate() {
        let (r, g, b, _) = crate::mapimage::PLAYER_COLORS[i % crate::mapimage::PLAYER_COLORS.len()];
        let mxc = avatar_mxcs.get(uid.as_str()).cloned().unwrap_or_default();
        let map_url = build_guess_map_url(
            display_name(names, uid),
            guess.lat,
            guess.lon,
            actual_lat,
            actual_lon,
            r,
            g,
            b,
            "en",
            &mxc,
            *score,
            *dist,
        );
        let label = crate::geocode::reverse_geocode(guess.lat, guess.lon, "en")
            .await
            .unwrap_or_else(|| format!("{:.2}, {:.2}", guess.lat, guess.lon));
        guess_labels.push(format!("[{label}]({map_url})"));
        overview_entries.push((
            uid.as_str(),
            guess.lat,
            guess.lon,
            r,
            g,
            b,
            *score,
            *dist,
            mxc,
        ));
    }

    // ── Build "all guesses" overview map URL ──────────────────────────────────
    let overview_url = if !overview_entries.is_empty() {
        build_all_guesses_map_url(&overview_entries, actual_lat, actual_lon, names)
    } else {
        maps_url.clone()
    };

    // ── Main-room reveal ──────────────────────────────────────────────────────
    let header_map_label = if scored.is_empty() {
        "Map"
    } else {
        "All guesses"
    };
    let mut lines = vec![
        format!(
            "📍 **{}** [{header_map_label}]({})",
            location_str, overview_url
        ),
        String::new(),
    ];

    if scored.is_empty() {
        lines.push("Nobody guessed.".to_owned());
    } else {
        for (i, ((uid, _guess, dist, score), guess_link)) in
            scored.iter().zip(guess_labels.iter()).enumerate()
        {
            let medal = match i {
                0 => "🥇",
                1 => "🥈",
                2 => "🥉",
                _ => "  ",
            };
            let dist_str = format_dist(*dist);
            lines.push(format!(
                "{medal} {} · {guess_link} · {} · {} pts",
                uid, dist_str, score,
            ));
        }
    }
    let text = lines.join("\n");

    // Matrix events have a 64 KB hard limit; body + formatted_body ≈ 2× text.
    // Tiers:
    //   1. Fits in one message → send as-is.
    //   2. Too big → summary + all individual links in one second message.
    //   3. Second message also too big → summary + one message per player.
    // Last resort: if even the summary alone is too large (30+ players all with
    // avatars), strip avatars from the overview URL.
    const MATRIX_SPLIT_THRESHOLD: usize = 28_000;
    if let Some(r) = client.get_room(&ctx.room_id) {
        if text.len() <= MATRIX_SPLIT_THRESHOLD || scored.is_empty() {
            r.send(format::mentionify(&text)).await.ok();
        } else {
            // Build summary (overview URL + scores, no per-player map links).
            let make_summary = |ov: &str| -> String {
                let mut sl = vec![
                    format!("📍 **{}** [{header_map_label}]({})", location_str, ov),
                    String::new(),
                ];
                for (i, (uid, _guess, dist, score)) in scored.iter().enumerate() {
                    let medal = match i {
                        0 => "🥇",
                        1 => "🥈",
                        2 => "🥉",
                        _ => "  ",
                    };
                    let dist_str = format_dist(*dist);
                    sl.push(format!("{medal} {} · {} · {} pts", uid, dist_str, score));
                }
                sl.join("\n")
            };
            let summary = make_summary(&overview_url);
            let summary = if summary.len() > MATRIX_SPLIT_THRESHOLD {
                // Last resort: rebuild overview URL without avatars.
                let stripped: Vec<(&str, f64, f64, u8, u8, u8, i64, f64, String)> =
                    overview_entries
                        .iter()
                        .map(|(uid, lat, lon, r, g, b, score, dist, _)| {
                            (*uid, *lat, *lon, *r, *g, *b, *score, *dist, String::new())
                        })
                        .collect();
                let slim_url = build_all_guesses_map_url(&stripped, actual_lat, actual_lon, names);
                make_summary(&slim_url)
            } else {
                summary
            };
            r.send(format::mentionify(&summary)).await.ok();

            // Build all individual map links as one block of text.
            let link_lines: Vec<String> = scored
                .iter()
                .zip(guess_labels.iter())
                .enumerate()
                .map(|(i, ((uid, _guess, dist, score), guess_link))| {
                    let medal = match i {
                        0 => "🥇",
                        1 => "🥈",
                        2 => "🥉",
                        _ => "  ",
                    };
                    let dist_str = format_dist(*dist);
                    format!(
                        "{medal} {} · {guess_link} · {} · {} pts",
                        uid, dist_str, score
                    )
                })
                .collect();
            let links_text = link_lines.join("\n");

            if links_text.len() <= MATRIX_SPLIT_THRESHOLD {
                r.send(format::mentionify(&links_text)).await.ok();
            } else {
                for line in &link_lines {
                    r.send(format::mentionify(line)).await.ok();
                }
            }
        }
    }

    // ── Main-room map images ──────────────────────────────────────────────────
    let map_mime: mime::Mime = "image/png".parse().unwrap();

    // 1. Winner's individual map (always shown when anyone guessed).
    if let Some((winner_uid, winner_guess, winner_dist, _)) = scored.first() {
        let (pr, pg, pb, _) = crate::mapimage::PLAYER_COLORS[0];
        let winner_raw = raw_avatars.get(winner_uid.as_str()).cloned();
        let (g_lat, g_lon, d) = (winner_guess.lat, winner_guess.lon, *winner_dist);
        if let Ok(Some(png)) = tokio::task::spawn_blocking(move || {
            let pin = crate::avatar::render_avatar_pin(winner_raw.as_deref(), pr, pg, pb);
            crate::mapimage::render_guess_map(
                g_lat, g_lon, actual_lat, actual_lon, d, pin, pr, pg, pb,
            )
        })
        .await
        {
            let (w, h) = image_dimensions(&png);
            let size = png.len();
            if let Ok(resp) = client.media().upload(&map_mime, png, None).await {
                let label = format!("🥇 Best guess · {} away", format_dist(d));
                if let Some(r) = client.get_room(&ctx.room_id) {
                    r.send(image_content_with_info(
                        label,
                        resp.content_uri,
                        &map_mime,
                        w,
                        h,
                        size,
                    ))
                    .await
                    .ok();
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
                (
                    display_name(names, uid).to_owned(),
                    guess.lat,
                    guess.lon,
                    raw,
                )
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
                    let pin = crate::avatar::render_avatar_pin(raw.as_deref(), pr, pg, pb);
                    (name, lat, lon, pin)
                })
                .collect();
            crate::mapimage::render_round_map(&guess_pins, actual_lat, actual_lon)
        })
        .await
        {
            let (w, h) = image_dimensions(&png);
            let size = png.len();
            if let Ok(resp) = client.media().upload(&map_mime, png, None).await {
                let label = format!("🗺️ All {} guesses this round", scored.len());
                if let Some(r) = client.get_room(&ctx.room_id) {
                    r.send(image_content_with_info(
                        label,
                        resp.content_uri,
                        &map_mime,
                        w,
                        h,
                        size,
                    ))
                    .await
                    .ok();

                    // Legend: 🔵 @alice:s  🔴 @bob:s  ⬛ actual
                    let mut parts: Vec<String> = scored
                        .iter()
                        .zip(legend.iter())
                        .map(|((uid, _, _, _), (_, emoji))| format!("{} {}", emoji, uid))
                        .collect();
                    parts.push("⬛ actual location".to_owned());
                    r.send(format::mentionify(&parts.join("   "))).await.ok();
                }
            }
        }
    }
}

async fn post_round_summary_free_guess(
    ctx: &BotContext,
    client: &Client,
    _room: &Room,
    round_id: i64,
    scores: &HashMap<String, i64>,
) {
    if scores.is_empty() {
        if let Some(r) = client.get_room(&ctx.room_id) {
            if let Ok(ev) = r
                .send(format::mentionify("🌍 Round over · nobody played."))
                .await
            {
                set_pinned(&r, &ev.response.event_id).await;
            }
        }
        return;
    }

    // ── Round results (this round only) ──────────────────────────────────────
    let n_guesses = ctx.effective_guesses_per_round().await;
    let max_pts = 5000i64 * n_guesses as i64;

    // Fetch per-user stats for this round from DB.
    let round_stats = ctx.db.round_stats(round_id).await.unwrap_or_default();

    const BAR_W: usize = 10;

    let mut lines = vec![
        format!(
            "🌍 **Round over!** {} guess(es) · max {} pts",
            n_guesses, max_pts
        ),
        String::new(),
    ];

    // Sort by total score desc, then avg distance asc (from DB), then username asc.
    let mut ranking: Vec<(&str, i64)> = scores.iter().map(|(u, &s)| (u.as_str(), s)).collect();
    ranking.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| {
                let da = round_stats
                    .iter()
                    .find(|e| e.user_id == a.0)
                    .map(|e| e.avg_distance_km)
                    .unwrap_or(f64::MAX);
                let db = round_stats
                    .iter()
                    .find(|e| e.user_id == b.0)
                    .map(|e| e.avg_distance_km)
                    .unwrap_or(f64::MAX);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .then(a.0.cmp(&b.0))
    });

    for (i, (uid, score)) in ranking.iter().enumerate() {
        let medal = match i {
            0 => "🥇",
            1 => "🥈",
            2 => "🥉",
            _ => "  ",
        };

        // Bar: fraction of max possible score.
        let filled = ((*score as f64 / max_pts as f64) * BAR_W as f64).round() as usize;
        let bar = format!(
            "{}{}",
            "█".repeat(filled.min(BAR_W)),
            "░".repeat(BAR_W - filled.min(BAR_W))
        );

        let pts_per_guess = if n_guesses > 0 {
            score / n_guesses as i64
        } else {
            0
        };

        // Distance stats from DB if available for this user.
        let (avg_dist, best_dist) = round_stats
            .iter()
            .find(|e| e.user_id == *uid)
            .map(|e| {
                (
                    format_dist(e.avg_distance_km),
                    format_dist(e.best_distance_km),
                )
            })
            .unwrap_or_else(|| ("n/a".to_owned(), "n/a".to_owned()));

        lines.push(format!(
            "{medal} {:>2}. {} : {} pts/guess",
            i + 1,
            uid,
            pts_per_guess
        ));
        lines.push(format!("      {bar}  ⌀ {}  🏅 {}", avg_dist, best_dist));
    }

    let round_text = lines.join("\n");

    if let Some(r) = client.get_room(&ctx.room_id) {
        if let Ok(ev) = r.send(format::mentionify(&round_text)).await {
            set_pinned(&r, &ev.response.event_id).await;
        }
    }

    // ── Leaderboards ──────────────────────────────────────────────────────────
    if let Some(r) = client.get_room(&ctx.room_id) {
        if let Some(lb_text) = crate::commands::build_alltime_leaderboard(ctx).await {
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
    let encoded: String = query
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => '+'.to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect();

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

/// Score: 5000 × e^(−distance_km / half_life_km).
/// Uses floor() so 5000 is only achievable at exactly 0 km.
/// Default half_life = 2000 km: 50 km → 4876, 500 km → 2840.
/// Lower half_life rewards precision: 1000 → 50 km scores 4753.
fn distance_score(distance_km: f64, half_life_km: f64) -> i64 {
    (5000.0 * (-distance_km / half_life_km.max(1.0)).exp()).floor() as i64
}

pub fn format_dist(dist_km: f64) -> String {
    if dist_km < 1.0 {
        format!("{:.0} m", dist_km * 1000.0)
    } else {
        format!("{:.0} km", dist_km)
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
        "🇩🇪" | "🇦🇹" => Some("de"),
        "🇺🇦" => Some("uk"),
        "🇫🇷" | "🇧🇪" | "🇲🇨" => Some("fr"),
        "🇪🇸" | "🇲🇽" | "🇦🇷" | "🇨🇴" | "🇨🇱" => Some("es"),
        "🇷🇺" | "🇧🇾" => Some("ru"),
        "🇮🇹" | "🇸🇲" => Some("it"),
        "🇵🇱" => Some("pl"),
        "🇳🇱" => Some("nl"),
        "🇵🇹" | "🇧🇷" => Some("pt"),
        "🇯🇵" => Some("ja"),
        "🇨🇳" | "🇹🇼" | "🇭🇰" => Some("zh"),
        "🇸🇦" | "🇦🇪" | "🇪🇬" | "🇲🇦" | "🇩🇿" => Some("ar"),
        "🇹🇷" => Some("tr"),
        "🇸🇪" => Some("sv"),
        "🇫🇮" => Some("fi"),
        "🇩🇰" => Some("da"),
        "🇨🇿" => Some("cs"),
        "🇭🇺" => Some("hu"),
        "🇷🇴" => Some("ro"),
        "🇬🇷" => Some("el"),
        "🇮🇱" => Some("he"),
        "🇰🇷" => Some("ko"),
        "🇹🇭" => Some("th"),
        "🇻🇳" => Some("vi"),
        "🇮🇩" | "🇲🇾" => Some("id"),
        _ => None,
    }
}

/// Map a BCP-47 language code string to the canonical code we store.
/// Accepts lowercase codes; returns `None` for unsupported codes.
pub fn text_code_to_lang(code: &str) -> Option<&'static str> {
    match code {
        "en" => Some("en"),
        "de" => Some("de"),
        "uk" => Some("uk"),
        "fr" => Some("fr"),
        "es" => Some("es"),
        "ru" => Some("ru"),
        "it" => Some("it"),
        "pl" => Some("pl"),
        "nl" => Some("nl"),
        "pt" => Some("pt"),
        "ja" => Some("ja"),
        "zh" => Some("zh"),
        "ar" => Some("ar"),
        "tr" => Some("tr"),
        "sv" => Some("sv"),
        "fi" => Some("fi"),
        "da" => Some("da"),
        "cs" => Some("cs"),
        "hu" => Some("hu"),
        "ro" => Some("ro"),
        "el" => Some("el"),
        "he" => Some("he"),
        "ko" => Some("ko"),
        "th" => Some("th"),
        "vi" => Some("vi"),
        "id" => Some("id"),
        _ => None,
    }
}

/// Human-readable name for a BCP-47 language code.
pub fn lang_label(code: &str) -> &'static str {
    match code {
        "en" => "English",
        "de" => "German",
        "uk" => "Ukrainian",
        "fr" => "French",
        "es" => "Spanish",
        "ru" => "Russian",
        "it" => "Italian",
        "pl" => "Polish",
        "nl" => "Dutch",
        "pt" => "Portuguese",
        "ja" => "Japanese",
        "zh" => "Chinese",
        "ar" => "Arabic",
        "tr" => "Turkish",
        "sv" => "Swedish",
        "fi" => "Finnish",
        "da" => "Danish",
        "cs" => "Czech",
        "hu" => "Hungarian",
        "ro" => "Romanian",
        "el" => "Greek",
        "he" => "Hebrew",
        "ko" => "Korean",
        "th" => "Thai",
        "vi" => "Vietnamese",
        "id" => "Indonesian",
        _ => "unknown",
    }
}

async fn reconcile_join_reactions(
    client: &Client,
    room: &Room,
    join_event_id: &OwnedEventId,
    bot_user_id: Option<&matrix_sdk::ruma::UserId>,
    participants: &mut HashSet<OwnedUserId>,
) {
    use matrix_sdk::ruma::{
        api::client::relations::get_relating_events_with_rel_type_and_event_type::v1 as api,
        events::{relation::RelationType, AnyMessageLikeEvent, TimelineEventType},
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
                    let Ok(AnyMessageLikeEvent::Reaction(ev)) = raw.deserialize() else {
                        continue;
                    };
                    let Some(orig) = ev.as_original() else {
                        continue;
                    };
                    if bot_user_id.map(|b| b == orig.sender).unwrap_or(false) {
                        continue;
                    }
                    if flag_to_lang(&orig.content.relates_to.key).is_some() {
                        participants.insert(orig.sender.clone());
                    }
                }
                match resp.next_batch {
                    Some(t) => from = Some(t),
                    None => break,
                }
            }
            Err(e) => {
                warn!("reconcile_join_reactions failed: {e}");
                break;
            }
        }
    }
}

// ── Image upload ──────────────────────────────────────────────────────────────

/// Upload the primary image and all extra images.
/// Returns a vec of `UploadedMedia` — primary first.
/// Extra images that fail to upload are silently skipped (logged as warnings).
async fn upload_all_images(client: &Client, img: &GeoImage) -> anyhow::Result<Vec<UploadedMedia>> {
    let primary = upload_image(client, img).await?;
    let mut results = vec![primary];

    for url in &img.extra_image_urls {
        match upload_http_url(client, url).await {
            Ok(r) => results.push(r),
            Err(e) => warn!("GeoGuessr: failed to upload extra image: {e}"),
        }
    }
    Ok(results)
}

/// Download an image from an HTTP(S) URL and upload it to the Matrix media store.
async fn upload_http_url(client: &Client, url: &str) -> anyhow::Result<UploadedMedia> {
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
    let (w, h) = image_dimensions(&data);
    let size = data.len();
    let response = client.media().upload(&mime, data, None).await?;
    Ok(UploadedMedia {
        uri: response.content_uri,
        mime,
        w,
        h,
        size,
    })
}

async fn upload_image(client: &Client, img: &GeoImage) -> anyhow::Result<UploadedMedia> {
    if img.image_url.starts_with('/') || img.image_url.starts_with("file://") {
        let path = img.image_url.trim_start_matches("file://");
        let data = tokio::fs::read(path).await?;
        let mime = detect_mime(&data);
        let (w, h) = image_dimensions(&data);
        let size = data.len();
        let response = client.media().upload(&mime, data, None).await?;
        Ok(UploadedMedia {
            uri: response.content_uri,
            mime,
            w,
            h,
            size,
        })
    } else {
        upload_http_url(client, &img.image_url).await
    }
}

fn detect_mime(data: &[u8]) -> mime::Mime {
    if data.starts_with(b"\x89PNG") {
        mime::IMAGE_PNG
    } else if data.starts_with(b"\xff\xd8\xff") {
        mime::IMAGE_JPEG
    } else {
        mime::IMAGE_JPEG
    }
}

/// Extract pixel dimensions from image bytes without full decode.
/// Returns (0, 0) if the format is unrecognised or the header is truncated.
fn image_dimensions(data: &[u8]) -> (u32, u32) {
    use std::io::Cursor;
    image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .unwrap_or((0, 0))
}

/// Build a spec-compliant `m.image` event with a populated `info` block.
/// Clients (especially Element X / Rust SDK) require `info.mimetype` to route
/// the event to the image renderer; without it they fall back to plain text body.
fn image_content_with_info(
    label: String,
    uri: OwnedMxcUri,
    mime: &mime::Mime,
    w: u32,
    h: u32,
    size: usize,
) -> RoomMessageEventContent {
    let ext = match mime.subtype().as_str() {
        "jpeg" => ".jpg",
        sub => {
            if matches!(sub, "png" | "webp" | "gif") {
                &format!(".{sub}")
            } else {
                ""
            }
        }
    };
    // Per Matrix spec: when `filename` differs from `body`, clients render
    // `body` as a visible caption and use `filename` only as the download name.
    // Setting both means users see the human-readable label, not a sanitized slug.
    let safe_filename = crate::format::sanitize_filename(&format!("{label}{ext}"));

    let mut info = ImageInfo::new();
    info.mimetype = Some(mime.to_string());
    info.width = if w > 0 { UInt::new(w as u64) } else { None };
    info.height = if h > 0 { UInt::new(h as u64) } else { None };
    info.size = if size > 0 {
        UInt::new(size as u64)
    } else {
        None
    };
    let mut content = ImageMessageEventContent::plain(label, uri);
    content.filename = Some(safe_filename);
    content.info = Some(Box::new(info));
    RoomMessageEventContent::new(MessageType::Image(content))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn time_bar(remaining: u64, total: u64) -> String {
    const W: usize = 10;
    let denom = total.max(1);
    let filled = ((remaining.min(denom) * W as u64) / denom) as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(W - filled))
}

/// Format a duration in seconds as a human-readable string.
/// Examples: 21600 → "6h", 5400 → "1h 30m", 90 → "1m 30s", 45 → "45s".
pub fn format_duration(secs: u64) -> String {
    if secs >= 3600 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    } else if secs >= 60 {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m {s}s")
        }
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
/// Compress `params` with raw DEFLATE and return a `#z=<base64url>` hash fragment.
///
/// Using the URL hash means the browser never sends the payload to the server,
/// so there is no length limit.  Raw DEFLATE (no zlib/gzip headers) matches the
/// browser's `DecompressionStream('deflate-raw')` API.  Base64url (no padding)
/// keeps the fragment URL-safe without further percent-encoding.
fn compress_to_hash(params: &str) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use flate2::{write::DeflateEncoder, Compression};
    use std::io::Write as _;

    let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
    enc.write_all(params.as_bytes()).expect("deflate write");
    let compressed = enc.finish().expect("deflate finish");
    format!("#z={}", URL_SAFE_NO_PAD.encode(&compressed))
}

/// Build a compressed single-player reveal URL (used for per-player DM links).
///
/// Inside the compressed payload the `g=` multi-guess format is used so the
/// browser parses it with the same code path as the overview map — no separate
/// `glat`/`glon` handling needed.  The `mxc` data: URL is included raw (no
/// percent-encoding) because it never contains `|` or `&`.
fn build_guess_map_url(
    name: &str,
    guess_lat: f64,
    guess_lon: f64,
    actual_lat: f64,
    actual_lon: f64,
    r: u8,
    g: u8,
    b: u8,
    lang: &str,
    mxc: &str,
    score: i64,
    dist_km: f64,
) -> String {
    let enc_name = url_encode_component(name);
    let params = format!(
        "lang={lang}&alat={actual_lat:.4}&alon={actual_lon:.4}\
         &g={enc_name}|{guess_lat:.4}|{guess_lon:.4}|{r:02X}{g:02X}{b:02X}|{mxc}|{score}|{dist_km:.3}"
    );
    format!(
        "https://polynomialdivision.github.io/geo-picker/reveal.html{}",
        compress_to_hash(&params)
    )
}

/// Build a compressed multi-player reveal URL.
///
/// Player entries are placed in the compressed payload using raw `|` separators
/// (no `%7C` escaping needed inside the compressed buffer) and the `mxc` data:
/// URL is included verbatim.  The structure text compresses well; the JPEG bytes
/// inside the base64 avatar strings do not, but they are already small (48 × 48,
/// JPEG q=55 ≈ 0.5–1 KB each).
/// Entry tuple: (uid, lat, lon, r, g, b, score, dist_km, mxc_data_url)
fn build_all_guesses_map_url(
    entries: &[(&str, f64, f64, u8, u8, u8, i64, f64, String)],
    actual_lat: f64,
    actual_lon: f64,
    names: &HashMap<String, String>,
) -> String {
    let mut params = format!("lang=en&alat={actual_lat:.4}&alon={actual_lon:.4}");
    for (uid, lat, lon, r, g, b, score, dist, mxc) in entries {
        let name = url_encode_component(display_name(names, uid));
        params.push_str(&format!(
            "&g={name}|{lat:.4}|{lon:.4}|{r:02X}{g:02X}{b:02X}|{mxc}|{score}|{dist:.3}"
        ));
    }
    format!(
        "https://polynomialdivision.github.io/geo-picker/reveal.html{}",
        compress_to_hash(&params)
    )
}

/// Percent-encode using the RFC 3986 unreserved-character set
/// (equivalent to JavaScript's `encodeURIComponent`).
fn url_encode_component(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Replace the room's pinned-messages list with just `event_id`.
/// Sending a single-element list atomically unpins all prior messages.
async fn set_pinned(room: &Room, event_id: &OwnedEventId) {
    let content = RoomPinnedEventsEventContent::new(vec![event_id.clone()]);
    if let Err(e) = room.send_state_event(content).await {
        warn!("Failed to pin message {event_id}: {e}");
    }
}

fn display_name<'a>(names: &'a HashMap<String, String>, uid: &'a str) -> &'a str {
    names
        .get(uid)
        .map(|s| s.as_str())
        .unwrap_or_else(|| uid.split(':').next().unwrap_or("").trim_start_matches('@'))
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

// ── Prefetch ──────────────────────────────────────────────────────────────────

pub async fn prefetch_if_needed(ctx: &BotContext, target: usize) {
    // Load played coords once — used for both cache eviction and new-image dedup.
    let db_coords: Vec<(f64, f64)> = ctx.db.recent_played_coords().await.unwrap_or_default();

    // Evict cached images that are now within MIN_DISTANCE_KM of a played location.
    // This prevents a location cached before a nearby round was played from
    // appearing again in a subsequent round.
    {
        let mut st = ctx.state.lock().await;
        let before = st.cached_guesses.len();
        st.cached_guesses.retain(|img| match img.lat.zip(img.lon) {
            None => true,
            Some((lat, lon)) => {
                crate::sources::min_dist_to_existing(lat, lon, &db_coords)
                    >= crate::sources::MIN_DISTANCE_KM
            }
        });
        let evicted = before - st.cached_guesses.len();
        if evicted > 0 {
            warn!(
                "GeoGuessr: evicted {evicted} stale cached image(s) too close to a played location"
            );
            st.save(&ctx.state_path).await.ok();
        }
    }

    // Only count cache entries that actually satisfy the *current*
    // photos-per-location requirement — an entry with too few photos will be
    // discarded by the pop-time dedup gate in `start_round` and is therefore
    // not "available" from the caller's point of view.  Counting raw cache
    // length here previously let the cache silently fill up with images that
    // could never be used once `photos_per_location` was raised, so prefetch
    // would think it had enough and stop fetching — permanently starving
    // every future round of usable images.
    let n_photos_needed = ctx.effective_photos_per_location().await;
    let (current, total_cached) = {
        let st = ctx.state.lock().await;
        let usable = st
            .cached_guesses
            .iter()
            .filter(|img| img.extra_image_urls.len() + 1 >= n_photos_needed)
            .count();
        (usable, st.cached_guesses.len())
    };
    let needed = target.saturating_sub(current);
    if needed == 0 {
        return;
    }

    info!(
        "GeoGuessr: prefetching {needed} image(s) (target={target}, usable_cached={current}, \
         total_cached={total_cached}, photos_per_location={n_photos_needed})"
    );

    let sources = &ctx.config.sources.enabled;

    // Collect existing coordinates and sequence IDs so each fetch in this
    // batch avoids duplicating locations already in the cache, fetched
    // earlier in the same batch, or played in recent rounds.
    let (mut existing_coords, mut existing_seqs): (Vec<(f64, f64)>, Vec<Option<String>>) = {
        let st = ctx.state.lock().await;
        st.cached_guesses
            .iter()
            .map(|img| {
                let coord = img.lat.zip(img.lon);
                (coord, img.sequence.clone())
            })
            .filter_map(|(coord, seq)| coord.map(|c| (c, seq)))
            .unzip()
    };
    // Merge in DB coords (deduplicating against cache coords already included).
    for coord in db_coords {
        if !existing_coords.contains(&coord) {
            existing_coords.push(coord);
        }
    }

    // Detect geographic collapse and enable exploration mode if needed.
    let diversity = crate::sources::diversity::DiversityTracker::from_coords(&existing_coords);
    let is_homogeneous = diversity.is_homogeneous();
    if is_homogeneous {
        warn!(
            "GeoGuessr: prefetch cache is geographically homogeneous — enabling exploration mode"
        );
    }

    // Restore the anti-starvation streak; enable exploration when homogeneous.
    let mut filter = crate::sources::quality_filter::FilterState::with_streak_and_exploration(
        ctx.prefetch_streak
            .load(std::sync::atomic::Ordering::Relaxed),
        is_homogeneous,
    );

    // Countries seen too often recently — skip at seed-selection time.
    // Combines DB history (last 90 days) with what is already in the cache.
    let skip_countries: HashSet<String> = {
        let mut freq: HashMap<String, u32> =
            ctx.db.recent_country_counts().await.unwrap_or_default();
        let st = ctx.state.lock().await;
        for img in &st.cached_guesses {
            *freq.entry(img.country.clone()).or_insert(0) += 1;
        }
        freq.into_iter()
            .filter(|(_, n)| *n >= 3)
            .map(|(k, _)| k)
            .collect()
    };

    for _ in 0..needed {
        let source = {
            let mut rng = rand::thread_rng();
            sources
                .choose(&mut rng)
                .map(|s| s.as_str())
                .unwrap_or("mapillary")
                .to_owned()
        };

        let result = match source.as_str() {
            "mapillary" => {
                crate::sources::mapillary::fetch(
                    &ctx.config.sources.mapillary,
                    n_photos_needed,
                    &existing_coords,
                    &existing_seqs,
                    &mut filter,
                    &ctx.blur_cache,
                    &skip_countries,
                )
                .await
            }
            "local" => match &ctx.config.sources.local.path {
                Some(p) => crate::sources::local::fetch(p).await,
                None => {
                    warn!("GeoGuessr: local source enabled but no path configured");
                    continue;
                }
            },
            _ => {
                warn!("GeoGuessr: unknown source '{}' — skipping", source);
                continue;
            }
        };

        match result {
            Ok(img) => {
                // Reset the starvation streak but keep exploration mode active
                // for the rest of this batch (homogeneity was computed upfront).
                filter = crate::sources::quality_filter::FilterState::with_streak_and_exploration(
                    0,
                    is_homogeneous,
                );
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

    // Persist the rejection streak for the next prefetch batch.
    ctx.prefetch_streak
        .store(filter.streak(), std::sync::atomic::Ordering::Relaxed);
}

// ── Resume after restart ──────────────────────────────────────────────────────

/// Called on startup when a join phase was in progress at shutdown time.
/// Waits until the game-start instant, re-reads reactions to rebuild the
/// participant list, then runs the game exactly as `start_round` would.
/// Resume a round that was interrupted by a restart.
pub async fn resume_active_round(ctx: BotContext, client: Client, ar: ActiveRoundState) {
    info!(
        "Resuming active round {} (guess {}/{})",
        ar.round_id, ar.guess_num, ar.total_guesses
    );

    let room = match client.get_room(&ctx.room_id) {
        Some(r) => r,
        None => {
            warn!("resume_active_round: main room not joined");
            return;
        }
    };

    // Rebuild sorted participant list from persisted state.
    let mut participants: Vec<OwnedUserId> = ar
        .dm_participants
        .keys()
        .filter_map(|s| s.parse::<OwnedUserId>().ok())
        .collect();
    participants.sort();

    room.send(format::mentionify(&format!(
        "⚠️ Bot restarted mid-round — replaying guess {}/{} with a fresh {} timer.",
        ar.guess_num,
        ar.total_guesses,
        format_duration(ar.answer_timeout_secs),
    )))
    .await
    .ok();

    let mut round_scores_free: HashMap<String, i64> = ar.round_scores.clone();

    if let Err(e) = play_free_guess(
        &ctx,
        &client,
        &room,
        ar.round_id,
        ar.guess_num,
        ar.total_guesses,
        &ar.current_image,
        &mut round_scores_free,
        &participants,
        ar.answer_timeout_secs,
    )
    .await
    {
        error!("resume_active_round: replayed guess error: {e}");
    }

    // ── Continue with remaining guesses ───────────────────────────────────────
    let mut image_queue = ar.remaining_images.clone();

    while let Some(img) = image_queue.pop_front() {
        let guess_num = ar.guess_num + (ar.remaining_images.len() - image_queue.len()) as u32;

        {
            let mut st = ctx.state.lock().await;
            let dm_state = participants
                .iter()
                .map(|uid| {
                    (
                        uid.to_string(),
                        ActiveDmParticipant {
                            dm_room_id: String::new(),
                            prompt_event_id: None,
                            answer_acked: false,
                        },
                    )
                })
                .collect();
            st.active_round = Some(ActiveRoundState {
                round_id: ar.round_id,
                guess_num,
                total_guesses: ar.total_guesses,
                current_image: img.clone(),
                remaining_images: image_queue.clone(),
                guess_started_at: chrono::Utc::now(),
                answer_timeout_secs: ar.answer_timeout_secs,
                dm_participants: dm_state,
                round_scores: round_scores_free.clone(),
            });
            st.save(&ctx.state_path).await.ok();
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(
            ctx.config.schedule.inter_guess_secs,
        ))
        .await;

        if let Err(e) = play_free_guess(
            &ctx,
            &client,
            &room,
            ar.round_id,
            guess_num,
            ar.total_guesses,
            &img,
            &mut round_scores_free,
            &participants,
            ar.answer_timeout_secs,
        )
        .await
        {
            error!("resume_active_round: play_free_guess error: {e}");
        }
    }

    // Finalise.
    ctx.db.finish_round(ar.round_id).await.ok();
    ctx.db
        .upsert_round_scores_free_guess(ar.round_id, &round_scores_free)
        .await
        .ok();

    post_round_summary_free_guess(&ctx, &client, &room, ar.round_id, &round_scores_free).await;

    {
        let mut st = ctx.state.lock().await;
        st.active_round = None;
        st.save(&ctx.state_path).await.ok();
    }
    *ctx.round_abort.lock().await = None;
}

pub async fn resume_pending_join(ctx: BotContext, client: Client, pj: PendingJoin) {
    // Wait until game_at_utc.
    let now_utc = chrono::Utc::now();
    let wait_secs = (pj.game_at_utc - now_utc)
        .max(chrono::Duration::zero())
        .num_seconds() as u64;
    if wait_secs > 0 {
        info!("resume_pending_join: waiting {wait_secs}s until game start");
        tokio::time::sleep(tokio::time::Duration::from_secs(wait_secs)).await;
    }

    let room = match client.get_room(&ctx.room_id) {
        Some(r) => r,
        None => {
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

    let n = ctx.effective_guesses_per_round().await;
    let n_photos = ctx.effective_photos_per_location().await;
    let triggered_by = "scheduler";

    if participants.is_empty() {
        room.send(format::mentionify("😴 Nobody joined · skipping."))
            .await
            .ok();
        if let Some(slot) = &pj.slot {
            let tz: Tz = ctx
                .config
                .schedule
                .timezone
                .parse()
                .unwrap_or(chrono_tz::UTC);
            let today = chrono::Utc::now().with_timezone(&tz).date_naive();
            let mut st = ctx.state.lock().await;
            st.last_game_dates.insert(slot.clone(), today);
            st.save(&ctx.state_path).await.ok();
        }
        return;
    }

    let mut sorted_participants: Vec<OwnedUserId> = participants.into_iter().collect();
    sorted_participants.sort();

    let participant_list: Vec<String> = sorted_participants.iter().map(|u| u.to_string()).collect();
    let participant_ids: Vec<OwnedUserId> = sorted_participants.clone();
    let n_players = sorted_participants.len();
    room.send(
        format::mentionify(&format!(
            "🌍 GeoGuessr starting now! {} player{}: {}",
            n_players,
            if n_players == 1 { "" } else { "s" },
            participant_list.join(", "),
        ))
        .add_mentions(Mentions::with_user_ids(participant_ids)),
    )
    .await
    .ok();

    // Pre-fetch and run images.
    prefetch_if_needed(&ctx, n + 2).await;

    let round_id = match ctx
        .db
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

    // Pre-collect all images into a queue (mirrors start_round) so that
    // active_round state can be persisted before each guess, making the
    // play phase of a resumed-join-round also crash-safe.
    let mut image_queue: VecDeque<GeoImage> = {
        let mut st = ctx.state.lock().await;
        let mut q = VecDeque::new();
        while q.len() < n {
            match st.cached_guesses.pop_front() {
                None => {
                    warn!(
                        "resume_pending_join: guess cache empty after {} image(s)",
                        q.len()
                    );
                    break;
                }
                Some(img) => {
                    if img.extra_image_urls.len() + 1 >= n_photos {
                        q.push_back(img);
                    } else {
                        warn!(
                            "GeoGuessr: pop-time dedup: discarded {} ({} photo(s) cached, {} required)",
                            img.country, img.extra_image_urls.len() + 1, n_photos,
                        );
                    }
                }
            }
        }
        st.save(&ctx.state_path).await.ok();
        q
    };

    if image_queue.is_empty() && n > 0 {
        abort_round_no_images(&ctx, &client, round_id, n).await;
        *ctx.round_abort.lock().await = None;
        return;
    }

    let total = image_queue.len();
    let mut round_scores_free: HashMap<String, i64> = HashMap::new();
    let mut first_guess = true;

    while let Some(img) = image_queue.pop_front() {
        let guess_num = (total - image_queue.len()) as u32; // 1-based after pop

        // Persist active_round before starting the guess so a second restart
        // during the play phase can resume via resume_active_round.
        {
            let mut st = ctx.state.lock().await;
            let dm_state = sorted_participants
                .iter()
                .map(|uid| {
                    (
                        uid.to_string(),
                        ActiveDmParticipant {
                            dm_room_id: String::new(),
                            prompt_event_id: None,
                            answer_acked: false,
                        },
                    )
                })
                .collect();
            st.active_round = Some(ActiveRoundState {
                round_id,
                guess_num,
                total_guesses: total as u32,
                current_image: img.clone(),
                remaining_images: image_queue.clone(),
                guess_started_at: chrono::Utc::now(),
                answer_timeout_secs: pj.answer_timeout_secs,
                dm_participants: dm_state,
                round_scores: round_scores_free.clone(),
            });
            st.save(&ctx.state_path).await.ok();
        }

        if !first_guess {
            tokio::time::sleep(tokio::time::Duration::from_secs(
                ctx.config.schedule.inter_guess_secs,
            ))
            .await;
        }
        first_guess = false;

        if let Err(e) = play_free_guess(
            &ctx,
            &client,
            &room,
            round_id,
            guess_num,
            total as u32,
            &img,
            &mut round_scores_free,
            &sorted_participants,
            pj.answer_timeout_secs,
        )
        .await
        {
            error!("resume_pending_join: play_free_guess error: {e}");
        }
    }

    ctx.db.finish_round(round_id).await.ok();
    ctx.db
        .upsert_round_scores_free_guess(round_id, &round_scores_free)
        .await
        .ok();

    // Clear active_round (round is complete) and record last-game date in
    // one atomic save so no intermediate state can trigger a spurious resume.
    {
        let mut st = ctx.state.lock().await;
        st.active_round = None;
        if let Some(slot) = &pj.slot {
            let tz: Tz = ctx
                .config
                .schedule
                .timezone
                .parse()
                .unwrap_or(chrono_tz::UTC);
            let today = chrono::Utc::now().with_timezone(&tz).date_naive();
            st.last_game_dates.insert(slot.clone(), today);
        }
        st.save(&ctx.state_path).await.ok();
    }

    post_round_summary_free_guess(&ctx, &client, &room, round_id, &round_scores_free).await;

    *ctx.round_abort.lock().await = None;
}

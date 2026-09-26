// Tests build fixtures by tweaking `Default` values field by field.
#![cfg_attr(test, allow(clippy::field_reassign_with_default))]

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{atomic::AtomicU32, Arc},
};

use anyhow::Result;
use mxbot_common::{
    admin::Dispatch,
    matrix_sdk::{
        deserialized_responses::EncryptionInfo,
        ruma::{
            events::{
                reaction::OriginalSyncReactionEvent,
                room::message::{MessageType, OriginalSyncRoomMessageEvent},
            },
            OwnedEventId, OwnedRoomId, OwnedUserId,
        },
        Client, Room, RoomState,
    },
    send::{in_thread, thread_root},
    Bot,
};
use tokio::sync::Mutex;
use tracing::{error, info};

mod avatar;
mod commands;
mod config;
mod countries;
mod db;
mod format;
mod game;
mod geocode;
mod mapimage;
mod scheduler;
mod sources;
mod state;
mod web;

use config::Config;
use game::ActiveGame;
use state::State;

// ── Join-phase state ──────────────────────────────────────────────────────────

/// Tracks who has opted in to the upcoming game round.
pub struct JoinState {
    /// Event ID of the "who wants to play?" message. None when no join phase is active.
    pub message_event_id: Option<OwnedEventId>,
    /// The emoji the bot reacted with (users who react with this count as opted-in).
    pub join_emoji: String,
    /// Users who have reacted with join_emoji (collected by the reaction handler).
    pub participants: HashSet<OwnedUserId>,
}

impl Default for JoinState {
    fn default() -> Self {
        JoinState {
            message_event_id: None,
            join_emoji: "👍".to_owned(),
            participants: HashSet::new(),
        }
    }
}

// ── Bot context ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct BotContext {
    pub state: Arc<Mutex<State>>,
    pub state_path: PathBuf,
    pub config: Arc<Config>,
    pub admin_users: HashSet<OwnedUserId>,
    pub room_id: OwnedRoomId,
    pub active_game: Arc<Mutex<Option<ActiveGame>>>,
    pub client: Client,
    pub db: Arc<db::Db>,
    /// Tracks participants who have opted in to the upcoming round.
    pub join_state: Arc<Mutex<JoinState>>,
    /// Abort handle for the currently running round task (join phase or active game).
    /// Set when a round is spawned; cleared when it finishes.
    pub round_abort: Arc<Mutex<Option<tokio::task::AbortHandle>>>,
    /// Consecutive Mapillary quality-filter rejections across prefetch sessions.
    /// Persists the anti-starvation streak so it survives between prefetch calls.
    pub prefetch_streak: Arc<AtomicU32>,
    /// Per-image-ID thumbnail metrics cache — avoids re-downloading thumbnails
    /// across prefetch sessions.  Stores (sharpness, overlay_penalty).
    /// Evicts oldest 20 % of entries at 1000-entry capacity.
    pub blur_cache: Arc<std::sync::Mutex<crate::sources::mapillary::BlurCache>>,
    /// In-memory per-player guess tokens; populated by play_free_guess, cleared
    /// at round end.  Always present; empty when web serving is disabled.
    pub web_tokens: web::SharedTokenStore,
    /// Public base URL of the web server (e.g. "https://geo.example.com").
    /// None when [web] is absent from config.
    pub web_public_url: Option<Arc<String>>,
}

impl BotContext {
    /// Effective guesses-per-round: schedule_overrides beats static config.
    /// Per-game overrides (ScheduledOnce) are handled at the call site.
    pub async fn effective_guesses_per_round(&self) -> usize {
        let ov = self.state.lock().await.schedule_overrides.guesses_per_round;
        ov.unwrap_or(self.config.schedule.guesses_per_round) as usize
    }

    /// Effective answer timeout: schedule_overrides beats static config.
    pub async fn effective_answer_timeout(&self) -> u64 {
        let ov = self
            .state
            .lock()
            .await
            .schedule_overrides
            .answer_timeout_secs;
        ov.unwrap_or(self.config.schedule.answer_timeout_secs)
    }

    /// Effective photos per guess location: schedule_overrides beats static config.
    pub async fn effective_photos_per_location(&self) -> usize {
        let ov = self
            .state
            .lock()
            .await
            .schedule_overrides
            .photos_per_location;
        ov.unwrap_or(self.config.schedule.photos_per_location)
    }
}

/// Run one `!command`; `None` when there is nothing to say.
async fn command_reply(ctx: &BotContext, sender: &OwnedUserId, body: &str) -> Option<String> {
    match commands::handle(ctx, sender, body).await {
        Ok(reply) => reply,
        Err(e) if e.is::<mxbot_common::admin::NotAdmin>() => {
            Some("❌ This command requires admin privileges.".to_owned())
        }
        Err(e) => {
            error!("Command error: {e}");
            None
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    mxbot_common::logging::init("geoguessr_bot");

    let config: Config =
        mxbot_common::config::load_toml(&mxbot_common::config::config_path_from_args())?;
    let config = Arc::new(config);

    let store_path = mxbot_common::config::store_path_from_env();
    tokio::fs::create_dir_all(&store_path).await?;

    let db = db::Db::open(&store_path.join("geo.db")).await?;
    db.migrate().await?;
    let db = Arc::new(db);

    let state_path = store_path.join("state.json");
    let mut st = State::load(&state_path).await?;
    if st.created_at.is_none() {
        st.created_at = Some(chrono::Utc::now());
        st.save(&state_path).await?;
    }
    let state = Arc::new(Mutex::new(st));

    let room_id =
        mxbot_common::rooms::parse_room_id("[schedule] room_id", &config.schedule.room_id)?;

    let bot = Bot::builder("geoguessr-bot", env!("CARGO_PKG_VERSION"))
        .store_path(&store_path)
        .admin_help(
            "!startgeo · !schedulegeo · !cancelgeo · !setschedule · !prefetch · !resetstats",
        )
        .start(&config.matrix, &config.security)
        .await?;
    let client = bot.client.clone();

    let join_state = Arc::new(Mutex::new(JoinState {
        join_emoji: config.schedule.join_emoji.clone(),
        ..Default::default()
    }));

    let web_tokens = web::new_token_store();
    let web_public_url = config.web.as_ref().map(|w| Arc::new(w.public_url.clone()));

    let ctx = BotContext {
        state,
        state_path,
        config: Arc::clone(&config),
        admin_users: bot.admins().clone(),
        room_id: room_id.clone(),
        active_game: Arc::new(Mutex::new(None)),
        client: client.clone(),
        db,
        join_state,
        round_abort: Arc::new(Mutex::new(None)),
        prefetch_streak: Arc::new(AtomicU32::new(0)),
        blur_cache: Arc::new(std::sync::Mutex::new(
            crate::sources::mapillary::BlurCache::new(1000),
        )),
        web_tokens: Arc::clone(&web_tokens),
        web_public_url: web_public_url.clone(),
    };

    // Spawn the web server if [web] is configured.
    if let Some(ref web_cfg) = config.web {
        let web_state = web::WebState {
            store: Arc::clone(&web_tokens),
            active_game: Arc::clone(&ctx.active_game),
            client: client.clone(),
            room_id: room_id.clone(),
            max_guesses: config.schedule.max_guesses_per_player,
            public_url: web_cfg.public_url.clone(),
        };
        let bind_addr = web_cfg.bind_addr.clone();
        tokio::spawn(async move {
            if let Err(e) = web::run(bind_addr, web_state).await {
                error!("Web server error: {e}");
            }
        });
    }

    // ── Message / command handler (main game room) ────────────────────────────
    client.add_event_handler({
        let ctx = ctx.clone();
        let bot = bot.clone();
        move |ev: OriginalSyncRoomMessageEvent,
              room: Room,
              client: Client,
              encryption: Option<EncryptionInfo>| {
            let ctx = ctx.clone();
            let bot = bot.clone();
            async move {
                if ev.sender == bot.user_id          { return; }
                if room.state() != RoomState::Joined { return; }
                match bot.admin.handle(&room, &ev, encryption.as_ref()).await {
                    Dispatch::Handled => return,
                    Dispatch::AdminDm => {
                        // Admin commands sent privately are answered privately.
                        let MessageType::Text(text) = &ev.content.msgtype else { return; };
                        if let Some(reply) = command_reply(&ctx, &ev.sender, text.body.trim()).await {
                            room.send(format::mentionify(&reply)).await.ok();
                        }
                        return;
                    }
                    Dispatch::Continue => {}
                }
                if room.room_id() != ctx.room_id     { return; }

                let MessageType::Text(ref text) = ev.content.msgtype else { return; };
                let body = text.body.trim();
                if !body.starts_with('!') { return; }

                let thread_root = thread_root(&ev);

                // Free-guess answer: !guess <location>  (fallback for main room)
                if body.to_lowercase().starts_with("!guess ") {
                    let query = body["!guess ".len()..].trim().to_owned();
                    if !query.is_empty() {
                        let ctx2   = ctx.clone();
                        let sender = ev.sender.clone();
                        let room2  = client.get_room(&ctx.room_id).clone();
                        tokio::spawn(async move {
                            let user      = sender.as_str().to_owned();
                            let max_g     = ctx2.config.schedule.max_guesses_per_player;
                            match game::geocode(&query).await {
                                Some((lat, lon)) => {
                                    // None = no guess open right now.
                                    let accepted = {
                                        let mut ag = ctx2.active_game.lock().await;
                                        ag.as_mut().map(|g| {
                                            g.record_free_guess(user, game::FreeGuess {
                                                text:         query.clone(),
                                                lat,
                                                lon,
                                                submitted_at: chrono::Utc::now(),
                                            }, max_g)
                                        })
                                    };
                                    if let Some(r) = room2 {
                                        let msg = match accepted {
                                            Some(true) => format!("✅ {sender} — guess recorded for \"{query}\""),
                                            Some(false) => format!("❌ {sender} — you already submitted a guess"),
                                            None => format!("⏳ {sender} — no guess is open right now"),
                                        };
                                        r.send(format::mentionify(&msg)).await.ok();
                                    }
                                }
                                None => {
                                    if let Some(r) = room2 {
                                        r.send(format::mentionify(&format!(
                                            "❓ {sender} — could not find \"{query}\", try a different name"
                                        )))
                                        .await
                                        .ok();
                                    }
                                }
                            }
                        });
                    }
                    return;
                }

                if let Some(reply) = command_reply(&ctx, &ev.sender, body).await {
                    if let Some(r) = client.get_room(&ctx.room_id) {
                        let content = in_thread(format::mentionify(&reply), thread_root, ev.event_id.clone());
                        r.send(content).await.ok();
                    }
                }
            }
        }
    });

    // ── Reaction handler — game answers + join opt-in ─────────────────────────
    client.add_event_handler({
        let ctx = ctx.clone();
        let bot_user_id = bot.user_id.clone();
        move |ev: OriginalSyncReactionEvent, room: Room, _client: Client| {
            let ctx = ctx.clone();
            let bot_user_id = bot_user_id.clone();
            async move {
                if ev.sender == bot_user_id {
                    return;
                }
                if room.state() != RoomState::Joined {
                    return;
                }
                if room.room_id() != ctx.room_id {
                    return;
                }

                let reacted_to = ev.content.relates_to.event_id.as_str().to_owned();
                let key = ev.content.relates_to.key.clone();

                // Check if this is a reaction to the join-phase message.
                let is_join_msg = {
                    let js = ctx.join_state.lock().await;
                    js.message_event_id.as_ref().map(|id| id.as_str()) == Some(&reacted_to)
                };

                if is_join_msg {
                    // Flag reaction → save language preference AND join the game.
                    let lang_pref = game::flag_to_lang(&key);
                    if let Some(lang) = lang_pref {
                        // Save language preference.
                        {
                            let mut st = ctx.state.lock().await;
                            st.user_langs
                                .insert(ev.sender.as_str().to_owned(), lang.to_owned());
                            st.save(&ctx.state_path).await.ok();
                        }
                        // Flag = join: add to participants and open DM.
                        {
                            let mut js = ctx.join_state.lock().await;
                            js.participants.insert(ev.sender.clone());
                        }
                    }
                }
            }
        }
    });

    // ── Initial sync ──────────────────────────────────────────────────────────
    bot.initial_sync().await;
    info!("Initial sync complete");

    // Pre-warm the image cache.
    {
        let ctx2 = ctx.clone();
        tokio::spawn(async move {
            game::prefetch_if_needed(&ctx2, ctx2.config.schedule.guesses_per_round as usize + 2)
                .await;
        });
    }

    // Resume a pending join phase that was interrupted by a restart.
    {
        let pending = ctx.state.lock().await.pending_join.clone();
        if let Some(pj) = pending {
            let now_utc = chrono::Utc::now();
            if pj.game_at_utc > now_utc {
                info!(
                    "Resuming pending join phase — game starts at {} UTC",
                    pj.game_at_utc.format("%H:%M")
                );
                // Restore in-memory JoinState so new reactions are still tracked.
                {
                    let mut js = ctx.join_state.lock().await;
                    if let Ok(eid) = pj.event_id.parse() {
                        js.message_event_id = Some(eid);
                    }
                    js.join_emoji = pj.join_emoji.clone();
                    js.participants.clear();
                }
                let ctx2 = ctx.clone();
                let client2 = client.clone();
                let handle = tokio::spawn(async move {
                    game::resume_pending_join(ctx2, client2, pj).await;
                });
                *ctx.round_abort.lock().await = Some(handle.abort_handle());
            } else {
                // Game time already passed — discard the stale pending join.
                info!("Discarding stale pending_join (game_at was in the past)");
                let mut st = ctx.state.lock().await;
                st.pending_join = None;
                st.save(&ctx.state_path).await.ok();
            }
        }
    }

    // Resume an active round interrupted by a restart.
    {
        let active = ctx.state.lock().await.active_round.clone();
        if let Some(ar) = active {
            info!(
                "Resuming active round {} (guess {}/{})",
                ar.round_id, ar.guess_num, ar.total_guesses
            );
            let ctx2 = ctx.clone();
            let client2 = client.clone();
            let handle = tokio::spawn(async move {
                game::resume_active_round(ctx2, client2, ar).await;
            });
            *ctx.round_abort.lock().await = Some(handle.abort_handle());
        }
    }

    tokio::spawn(scheduler::run(ctx, client.clone()));

    bot.run().await
}

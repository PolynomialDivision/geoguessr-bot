use std::{collections::{HashMap, HashSet}, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use matrix_sdk::{
    Client, Room, RoomState,
    config::SyncSettings,
    ruma::{
        OwnedEventId, OwnedRoomId, OwnedServerName, OwnedUserId, RoomOrAliasId,
        api::client::filter::FilterDefinition,
        events::{
            key::verification::request::ToDeviceKeyVerificationRequestEvent,
            reaction::OriginalSyncReactionEvent,
            room::{
                member::StrippedRoomMemberEvent,
                message::{
                    MessageType, OriginalSyncRoomMessageEvent,
                    Relation,
                },
            },
        },
    },
};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

mod commands;
mod config;
mod countries;
mod db;
mod format;
mod game;
mod scheduler;
mod sources;
mod state;

use config::Config;
use game::{ActiveGame, ActiveGameMode};
use state::State;

// ── Join-phase state ──────────────────────────────────────────────────────────

/// Tracks who has opted in to the upcoming game round.
pub struct JoinState {
    /// Event ID of the "who wants to play?" message. None when no join phase is active.
    pub message_event_id: Option<OwnedEventId>,
    /// The emoji the bot reacted with (users who react with this count as opted-in).
    pub join_emoji:       String,
    /// Users who have reacted with join_emoji (collected by the reaction handler).
    pub participants:     HashSet<OwnedUserId>,
}

impl Default for JoinState {
    fn default() -> Self {
        JoinState {
            message_event_id: None,
            join_emoji:       "👍".to_owned(),
            participants:     HashSet::new(),
        }
    }
}

// ── Bot context ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct BotContext {
    pub state:       Arc<Mutex<State>>,
    pub state_path:  PathBuf,
    pub config:      Arc<Config>,
    pub admin_users: HashSet<OwnedUserId>,
    pub room_id:     OwnedRoomId,
    pub active_game: Arc<Mutex<Option<ActiveGame>>>,
    pub client:      Client,
    pub db:          Arc<db::Db>,
    /// Tracks participants who have opted in to the upcoming round.
    pub join_state:  Arc<Mutex<JoinState>>,
    /// Maps DM room IDs to the user_id of the participant (for routing DM answers).
    pub dm_rooms:    Arc<Mutex<HashMap<OwnedRoomId, OwnedUserId>>>,
}

fn thread_reply(text: &str, root: matrix_sdk::ruma::OwnedEventId) -> matrix_sdk::ruma::events::room::message::RoomMessageEventContent {
    use matrix_sdk::ruma::events::{relation::Thread, room::message::Relation};
    let mut content = format::mentionify(text);
    content.relates_to = Some(Relation::Thread(Thread::reply(root.clone(), root)));
    content
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "geoguessr_bot=info,matrix_sdk=warn".parse().unwrap()),
        )
        .init();

    let config_path = std::env::args()
        .find(|a| a.ends_with(".toml"))
        .unwrap_or_else(|| "config.toml".to_owned());
    let config: Config = toml::from_str(
        &std::fs::read_to_string(&config_path)
            .with_context(|| format!("Reading config {config_path}"))?,
    )
    .context("Parsing config")?;
    let config = Arc::new(config);

    let store_path = PathBuf::from(
        std::env::var("STORE_PATH").unwrap_or_else(|_| "store".to_owned()),
    );
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

    let admin_users: HashSet<OwnedUserId> = config.security.admin_users
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    let allowed_inviters: HashSet<String> = config.security.allowed_inviters
        .iter()
        .cloned()
        .collect();

    let room_id: OwnedRoomId = config.schedule.room_id
        .parse()
        .context("Invalid room_id in [schedule]")?;

    let (client, bot_user_id) = mxbot_common::session::build_and_restore(
        &config.matrix,
        &store_path,
        config.security.encryption_strategy.clone().into(),
    )
    .await?;

    let join_state = Arc::new(Mutex::new(JoinState {
        join_emoji: config.schedule.join_emoji.clone(),
        ..Default::default()
    }));

    let ctx = BotContext {
        state,
        state_path,
        config:      Arc::clone(&config),
        admin_users,
        room_id:     room_id.clone(),
        active_game: Arc::new(Mutex::new(None)),
        client:      client.clone(),
        db,
        join_state,
        dm_rooms:    Arc::new(Mutex::new(HashMap::new())),
    };

    // ── Invite handler ────────────────────────────────────────────────────────
    client.add_event_handler({
        let allowed_inviters = allowed_inviters.clone();
        let bot_user_id      = bot_user_id.clone();
        move |ev: StrippedRoomMemberEvent, room: Room, client: Client| {
            let allowed_inviters = allowed_inviters.clone();
            let bot_user_id      = bot_user_id.clone();
            async move {
                if ev.state_key != bot_user_id { return; }
                if !allowed_inviters.is_empty()
                    && !allowed_inviters.contains(ev.sender.as_str())
                {
                    warn!("Rejecting invite from {}", ev.sender);
                    room.leave().await.ok();
                    return;
                }
                let room_id = room.room_id().to_owned();
                let mut via: Vec<OwnedServerName> = vec![ev.sender.server_name().to_owned()];
                if let Some(s) = room_id.server_name() {
                    let s = s.to_owned();
                    if !via.contains(&s) { via.push(s); }
                }
                if let Ok(roa) = RoomOrAliasId::parse(room_id.as_str()) {
                    if let Err(e) = client.join_room_by_id_or_alias(&roa, &via).await {
                        warn!("Join failed: {e}");
                    }
                }
            }
        }
    });

    // ── Message / command handler (main game room) ────────────────────────────
    client.add_event_handler({
        let ctx         = ctx.clone();
        let bot_user_id = bot_user_id.clone();
        move |ev: OriginalSyncRoomMessageEvent, room: Room, client: Client| {
            let ctx         = ctx.clone();
            let bot_user_id = bot_user_id.clone();
            async move {
                if ev.sender == bot_user_id          { return; }
                if room.state() != RoomState::Joined { return; }
                if room.room_id() != ctx.room_id     { return; }

                let MessageType::Text(ref text) = ev.content.msgtype else { return; };
                let body = text.body.trim();
                if !body.starts_with('!') { return; }

                let thread_root = match &ev.content.relates_to {
                    Some(Relation::Thread(t)) => t.event_id.clone(),
                    _                         => ev.event_id.clone(),
                };

                // Multiple-choice answer shorthand: !a / !b / !c / !d
                let answer_index: Option<u8> = match body.to_lowercase().as_str() {
                    "!a" => Some(0), "!b" => Some(1),
                    "!c" => Some(2), "!d" => Some(3),
                    _    => None,
                };
                if let Some(choice_index) = answer_index {
                    let user = ev.sender.as_str().to_owned();
                    let mut ag = ctx.active_game.lock().await;
                    if let Some(g) = ag.as_mut() {
                        g.record_mc_answer(user, choice_index, "text");
                    }
                    return;
                }

                // Free-guess answer: !guess <location>  (fallback for main room)
                if body.to_lowercase().starts_with("!guess ") {
                    let query = body["!guess ".len()..].trim().to_owned();
                    if !query.is_empty() {
                        let ctx2   = ctx.clone();
                        let sender = ev.sender.clone();
                        let room2  = client.get_room(&ctx.room_id).clone();
                        tokio::spawn(async move {
                            let user = sender.as_str().to_owned();
                            match game::geocode(&query).await {
                                Some((lat, lon)) => {
                                    let mut ag = ctx2.active_game.lock().await;
                                    if let Some(g) = ag.as_mut() {
                                        g.record_free_guess(user, game::FreeGuess {
                                            text:         query.clone(),
                                            lat,
                                            lon,
                                            submitted_at: chrono::Utc::now(),
                                        });
                                    }
                                    if let Some(r) = room2 {
                                        r.send(format::mentionify(&format!(
                                            "✅ {sender} — guess recorded for \"{query}\""
                                        )))
                                        .await
                                        .ok();
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

                match commands::handle(&ctx, &ev.sender, body).await {
                    Ok(Some(reply)) => {
                        if let Some(r) = client.get_room(&ctx.room_id) {
                            r.send(thread_reply(&reply, thread_root)).await.ok();
                        }
                    }
                    Err(e) if e.to_string() == "__not_admin__" => {
                        if let Some(r) = client.get_room(&ctx.room_id) {
                            r.send(thread_reply(
                                "❌ This command requires admin privileges.",
                                thread_root,
                            ))
                            .await
                            .ok();
                        }
                    }
                    Ok(None) => {}
                    Err(e)   => error!("Command error: {e}"),
                }
            }
        }
    });

    // ── DM answer handler (free-guess answers sent in private chat) ───────────
    client.add_event_handler({
        let ctx         = ctx.clone();
        let bot_user_id = bot_user_id.clone();
        move |ev: OriginalSyncRoomMessageEvent, room: Room, client: Client| {
            let ctx         = ctx.clone();
            let bot_user_id = bot_user_id.clone();
            async move {
                if ev.sender == bot_user_id          { return; }
                if room.state() != RoomState::Joined { return; }
                // Only handle rooms that are NOT the main game room.
                if room.room_id() == ctx.room_id     { return; }

                // Is this one of our game DM rooms?
                let user_id = {
                    let dm_rooms = ctx.dm_rooms.lock().await;
                    dm_rooms.get(room.room_id()).cloned()
                };
                let Some(user_id) = user_id else { return; };

                let MessageType::Text(ref text) = ev.content.msgtype else { return; };
                // Skip command-looking messages (e.g. !help, !scores).
                let body = text.body.trim();
                if body.starts_with('!') { return; }
                if body.is_empty()       { return; }

                let query = body.to_owned();
                let dm_room_id = room.room_id().to_owned();

                // There must be an active free-guess game.
                let is_free_guess = {
                    let ag = ctx.active_game.lock().await;
                    matches!(
                        ag.as_ref().map(|g| &g.mode),
                        Some(ActiveGameMode::FreeGuess { .. })
                    )
                };
                if !is_free_guess { return; }

                tokio::spawn(async move {
                    match game::geocode(&query).await {
                        Some((lat, lon)) => {
                            {
                                let mut ag = ctx.active_game.lock().await;
                                if let Some(g) = ag.as_mut() {
                                    g.record_free_guess(
                                        user_id.as_str().to_owned(),
                                        game::FreeGuess {
                                            text:         query.clone(),
                                            lat,
                                            lon,
                                            submitted_at: chrono::Utc::now(),
                                        },
                                    );
                                }
                            }
                            if let Some(r) = client.get_room(&dm_room_id) {
                                r.send(format::mentionify(&format!(
                                    "✅ Guess recorded: \"{query}\"\nWaiting for the timer to end…"
                                )))
                                .await
                                .ok();
                            }
                        }
                        None => {
                            if let Some(r) = client.get_room(&dm_room_id) {
                                r.send(format::mentionify(&format!(
                                    "❓ Could not geocode \"{query}\" — try a full address, city name, or lat,lon"
                                )))
                                .await
                                .ok();
                            }
                        }
                    }
                });
            }
        }
    });

    // ── Reaction handler — game answers + join opt-in ─────────────────────────
    client.add_event_handler({
        let ctx         = ctx.clone();
        let bot_user_id = bot_user_id.clone();
        move |ev: OriginalSyncReactionEvent, room: Room, client: Client| {
            let ctx         = ctx.clone();
            let bot_user_id = bot_user_id.clone();
            async move {
                if ev.sender == bot_user_id          { return; }
                if room.state() != RoomState::Joined { return; }
                if room.room_id() != ctx.room_id     { return; }

                let reacted_to = ev.content.relates_to.event_id.as_str().to_owned();
                let key        = ev.content.relates_to.key.clone();

                // Check if this is a join opt-in reaction.
                {
                    let mut js = ctx.join_state.lock().await;
                    if js.message_event_id.as_ref().map(|id| id.as_str()) == Some(&reacted_to)
                        && key == js.join_emoji
                    {
                        let is_new        = js.participants.insert(ev.sender.clone());
                        let reminder_secs = ctx.config.schedule.reminder_before_secs;
                        drop(js);

                        // Immediately open (or reuse) a DM and send a confirmation.
                        if is_new {
                            let sender  = ev.sender.clone();
                            let ctx2    = ctx.clone();
                            let client2 = client.clone();
                            tokio::spawn(async move {
                                game::open_join_dm(&ctx2, &client2, &sender, reminder_secs).await;
                            });
                        }
                        return;
                    }
                }

                // Otherwise treat as a multiple-choice game answer.
                let choice_index = match key.as_str() {
                    "🇦" => 0u8, "🇧" => 1, "🇨" => 2, "🇩" => 3, _ => return,
                };
                let user = ev.sender.as_str().to_owned();
                let mut ag = ctx.active_game.lock().await;
                if let Some(game) = ag.as_mut() {
                    if game.event_id.as_str() == reacted_to {
                        game.record_mc_answer(user, choice_index, "reaction");
                    }
                }
            }
        }
    });

    // ── Verification handler ──────────────────────────────────────────────────
    client.add_event_handler({
        let reset_allowed: Arc<Mutex<HashSet<OwnedUserId>>> =
            Arc::new(Mutex::new(HashSet::new()));
        move |ev: ToDeviceKeyVerificationRequestEvent, client: Client| {
            let reset = Arc::clone(&reset_allowed);
            async move {
                if let Some(req) = client
                    .encryption()
                    .get_verification_request(&ev.sender, &ev.content.transaction_id)
                    .await
                {
                    tokio::spawn(mxbot_common::verify::handle_verification_request(
                        client, reset, req,
                    ));
                }
            }
        }
    });

    // ── Initial sync ──────────────────────────────────────────────────────────
    let filter = FilterDefinition::with_lazy_loading();
    client
        .sync_once(SyncSettings::default().filter(filter.into()))
        .await
        .context("Initial sync failed")?;
    info!("Initial sync complete");

    // Pre-warm the image cache.
    {
        let ctx2 = ctx.clone();
        tokio::spawn(async move {
            game::prefetch_if_needed(
                &ctx2,
                ctx2.config.schedule.images_per_round as usize + 2,
            )
            .await;
        });
    }

    tokio::spawn(scheduler::run(ctx, client.clone()));

    loop {
        match client.sync(SyncSettings::default()).await {
            Ok(()) => warn!("Sync loop exited — reconnecting"),
            Err(e) => {
                warn!("Sync error: {e} — reconnecting in 5s");
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    }
}

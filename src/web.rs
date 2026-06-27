//! HTTP server for interactive map-based guess submission.
//!
//! Each DM participant receives a personal token URL posted in the game room.
//! Opening the URL shows a Leaflet world map.  Clicking places a draggable
//! pin; pressing Submit records the guess.  Tokens expire at round end.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderValue, Method, StatusCode, header},
    response::{Html, IntoResponse, Json, Redirect, Response},
    routing::{get, post},
};
use tower_http::cors::CorsLayer;
use matrix_sdk::{
    Client,
    ruma::{OwnedEventId, OwnedRoomId, OwnedUserId},
};
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, sync::Mutex};
use tracing::{info, warn};

use crate::game::{ActiveGame, ActiveGameMode, FreeGuess};

// ── Token + session store ─────────────────────────────────────────────────────

/// One record per player per guess.
#[derive(Clone, Debug)]
pub struct GuessToken {
    pub user_id:   OwnedUserId,
    pub round_id:  i64,
    pub guess_num: u32,
    pub lang:      String,
}

/// Per-guess session shared across all tokens for the same (round_id, guess_num).
#[derive(Clone, Debug)]
pub struct GuessSession {
    /// Event ID of the links message in the game room — edited on each submission.
    pub links_event_id: OwnedEventId,
    /// Ordered participant list (controls display order in status line).
    pub participants:   Vec<OwnedUserId>,
}

#[derive(Default)]
pub struct TokenStore {
    pub tokens:   HashMap<String, GuessToken>,
    pub sessions: HashMap<(i64, u32), GuessSession>,
}

pub type SharedTokenStore = Arc<Mutex<TokenStore>>;

pub fn new_token_store() -> SharedTokenStore {
    Arc::new(Mutex::new(TokenStore::default()))
}

// ── Axum shared state ─────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct WebState {
    pub store:       SharedTokenStore,
    pub active_game: Arc<Mutex<Option<ActiveGame>>>,
    pub client:      Client,
    pub room_id:     OwnedRoomId,
    /// 0 = unlimited updates; >0 = only the first submission counts.
    pub max_guesses: u32,
    /// Public base URL (no trailing slash).
    pub public_url:  String,
}

// ── Server entry point ────────────────────────────────────────────────────────

pub async fn run(bind_addr: String, state: WebState) -> anyhow::Result<()> {
    let cors = CorsLayer::new()
        .allow_origin("https://polynomialdivision.github.io".parse::<HeaderValue>().unwrap())
        .allow_methods([Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE]);

    let app = Router::new()
        .route("/g/:token",        get(serve_map))
        .route("/g/:token/submit", post(submit_guess))
        .layer(cors)
        .with_state(state);

    let listener = TcpListener::bind(&bind_addr).await?;
    info!("Geo-guess web server listening on {bind_addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

// ── Map page ──────────────────────────────────────────────────────────────────

async fn serve_map(
    Path(token): Path<String>,
    State(ws):   State<WebState>,
) -> Response {
    let info = {
        let store = ws.store.lock().await;
        store.tokens.get(&token).cloned()
    };
    let Some(tok) = info else {
        return (StatusCode::NOT_FOUND, Html(expired_html())).into_response();
    };
    let once = if ws.max_guesses > 0 { "1" } else { "0" };
    let encoded_base = percent_encode(&ws.public_url);
    let url = format!(
        "https://polynomialdivision.github.io/geo-picker/?lang={}&token={}&base={}&once={}",
        tok.lang, token, encoded_base, once
    );
    Redirect::to(&url).into_response()
}

// ── Guess submission ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SubmitBody {
    lat: f64,
    lon: f64,
}

#[derive(Serialize)]
struct SubmitResponse {
    geocoded: Option<String>,
}

async fn submit_guess(
    Path(token): Path<String>,
    State(ws):   State<WebState>,
    Json(body):  Json<SubmitBody>,
) -> Response {
    if !is_valid_coords(body.lat, body.lon) {
        return (StatusCode::BAD_REQUEST, "Invalid coordinates").into_response();
    }

    let tok = {
        let store = ws.store.lock().await;
        match store.tokens.get(&token).cloned() {
            Some(t) => t,
            None    => return (StatusCode::NOT_FOUND, "Link expired or invalid").into_response(),
        }
    };

    let already_submitted = {
        let ag = ws.active_game.lock().await;
        ag.as_ref().map_or(false, |g| {
            let ActiveGameMode::FreeGuess { ref guesses, .. } = g.mode;
            guesses.contains_key(tok.user_id.as_str())
        })
    };

    // One-shot: acknowledge silently but don't overwrite the original guess.
    if already_submitted && ws.max_guesses > 0 {
        let geocoded = crate::geocode::reverse_geocode(body.lat, body.lon, &tok.lang).await;
        return (StatusCode::OK, Json(SubmitResponse { geocoded })).into_response();
    }

    let accepted = {
        let mut ag = ws.active_game.lock().await;
        ag.as_mut().map_or(false, |g| {
            g.record_free_guess(
                tok.user_id.as_str().to_owned(),
                FreeGuess {
                    text:         format!("{:.4},{:.4}", body.lat, body.lon),
                    lat:          body.lat,
                    lon:          body.lon,
                    submitted_at: chrono::Utc::now(),
                },
                ws.max_guesses,
            )
        })
    };

    info!(
        "Web guess: {} → ({:.4}, {:.4}) accepted={}",
        tok.user_id, body.lat, body.lon, accepted
    );

    if accepted {
        update_links_message(&ws, tok.round_id, tok.guess_num).await;
    }

    let geocoded = crate::geocode::reverse_geocode(body.lat, body.lon, &tok.lang).await;
    (StatusCode::OK, Json(SubmitResponse { geocoded })).into_response()
}

// ── Links message refresh ─────────────────────────────────────────────────────

/// Edit the "🗺️ Alice · Bob · Carol" message in the game room to reflect
/// who has submitted (✅) vs who is still pending (clickable map link).
pub async fn update_links_message(ws: &WebState, round_id: i64, guess_num: u32) {
    let (session, token_by_user) = {
        let store = ws.store.lock().await;
        let session = match store.sessions.get(&(round_id, guess_num)).cloned() {
            Some(s) => s,
            None    => return,
        };
        // Map user_id → (token, lang) for building geo-picker links.
        let token_by_user: HashMap<String, (String, String)> = store.tokens.iter()
            .filter(|(_, t)| t.round_id == round_id && t.guess_num == guess_num)
            .map(|(tok, t)| (t.user_id.to_string(), (tok.clone(), t.lang.clone())))
            .collect();
        (session, token_by_user)
    };

    let submitted: HashSet<String> = {
        let ag = ws.active_game.lock().await;
        ag.as_ref().map_or_else(HashSet::new, |g| {
            let ActiveGameMode::FreeGuess { ref guesses, .. } = g.mode;
            guesses.keys().cloned().collect()
        })
    };

    let once = if ws.max_guesses > 0 { "1" } else { "0" };
    let encoded_base = percent_encode(&ws.public_url);
    let line = session.participants.iter().map(|uid| {
        if submitted.contains(uid.as_str()) {
            format!("✅ {}", uid.localpart())
        } else if let Some((tok, lang)) = token_by_user.get(uid.as_str()) {
            format!(
                "[🗺️ {}](https://polynomialdivision.github.io/geo-picker/?lang={}&token={}&base={}&once={})",
                uid.localpart(), lang, tok, encoded_base, once
            )
        } else {
            format!("⏳ {}", uid.localpart())
        }
    }).collect::<Vec<_>>().join("  ·  ");

    let Some(room) = ws.client.get_room(&ws.room_id) else { return };
    use matrix_sdk::ruma::events::room::message::ReplacementMetadata;
    let edit = crate::format::mentionify(&line)
        .make_replacement(ReplacementMetadata::new(session.links_event_id.clone(), None));
    if let Err(e) = room.send(edit).await {
        warn!("Failed to update links message: {e}");
    }
}

// ── Token generation ──────────────────────────────────────────────────────────

pub fn generate_token() -> String {
    use rand::Rng;
    rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(24)
        .map(char::from)
        .collect()
}

/// Percent-encode a string for use as a URL query parameter value.
pub fn percent_encode(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

// ── Coordinate validation ─────────────────────────────────────────────────────

pub fn is_valid_coords(lat: f64, lon: f64) -> bool {
    lat.is_finite() && lon.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0
}

// ── HTML ──────────────────────────────────────────────────────────────────────

fn expired_html() -> String {
    r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><title>Link expired</title>
<style>
body{background:#1a1a2e;color:#e0e0f0;font-family:system-ui,sans-serif;
  display:flex;align-items:center;justify-content:center;height:100vh;margin:0}
div{text-align:center}
h2{font-size:1.4rem;margin-bottom:12px}
p{color:#99aacc;font-size:0.95rem}
</style></head>
<body><div>
<h2>⏰ This link has expired</h2>
<p>The round has ended or the link is invalid.</p>
</div></body></html>"#.to_owned()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use matrix_sdk::ruma::OwnedUserId;
    use crate::game::{ActiveGame, ActiveGameMode};

    // ── Token generation ──────────────────────────────────────────────────────

    #[test]
    fn token_is_24_alphanumeric_chars() {
        let t = generate_token();
        assert_eq!(t.len(), 24, "token must be 24 chars");
        assert!(t.chars().all(|c| c.is_ascii_alphanumeric()), "token must be alphanumeric");
    }

    #[test]
    fn tokens_are_unique() {
        let tokens: Vec<String> = (0..200).map(|_| generate_token()).collect();
        let set: HashSet<&String> = tokens.iter().collect();
        assert_eq!(set.len(), tokens.len(), "all 200 generated tokens must be distinct");
    }

    // ── Token store ───────────────────────────────────────────────────────────

    #[test]
    fn token_store_add_and_retrieve() {
        let uid: OwnedUserId = "@alice:example.com".try_into().unwrap();
        let mut store = TokenStore::default();
        store.tokens.insert("tok1".to_owned(), GuessToken {
            user_id: uid.clone(), round_id: 1, guess_num: 1, lang: "en".to_owned(),
        });
        let found = store.tokens.get("tok1");
        assert!(found.is_some());
        assert_eq!(found.unwrap().user_id, uid);
        assert!(store.tokens.get("wrong_token").is_none());
    }

    #[test]
    fn token_store_clear_by_round_and_guess() {
        let uid: OwnedUserId = "@alice:example.com".try_into().unwrap();
        let mut store = TokenStore::default();
        store.tokens.insert("t_r1g1".to_owned(), GuessToken { user_id: uid.clone(), round_id: 1, guess_num: 1, lang: "en".to_owned() });
        store.tokens.insert("t_r1g2".to_owned(), GuessToken { user_id: uid.clone(), round_id: 1, guess_num: 2, lang: "en".to_owned() });
        store.tokens.insert("t_r2g1".to_owned(), GuessToken { user_id: uid.clone(), round_id: 2, guess_num: 1, lang: "en".to_owned() });

        // Simulate clearing round 1, guess 1.
        store.tokens.retain(|_, t| !(t.round_id == 1 && t.guess_num == 1));
        store.sessions.remove(&(1, 1));

        assert!(!store.tokens.contains_key("t_r1g1"), "cleared token must be gone");
        assert!(store.tokens.contains_key("t_r1g2"), "other guess token must survive");
        assert!(store.tokens.contains_key("t_r2g1"), "other round token must survive");
    }

    #[test]
    fn token_store_full_clear() {
        let uid: OwnedUserId = "@alice:example.com".try_into().unwrap();
        let mut store = TokenStore::default();
        for i in 0..5 {
            store.tokens.insert(format!("tok{i}"), GuessToken { user_id: uid.clone(), round_id: 1, guess_num: i, lang: "en".to_owned() });
        }
        store.tokens.clear();
        store.sessions.clear();
        assert!(store.tokens.is_empty());
        assert!(store.sessions.is_empty());
    }

    // ── Coordinate validation ─────────────────────────────────────────────────

    #[test]
    fn coord_validation_accepts_valid() {
        assert!(is_valid_coords(0.0,   0.0));
        assert!(is_valid_coords(90.0,  180.0));
        assert!(is_valid_coords(-90.0, -180.0));
        assert!(is_valid_coords(48.13, 11.58));
        assert!(is_valid_coords(-33.87, 151.21));
    }

    #[test]
    fn coord_validation_rejects_out_of_range() {
        assert!(!is_valid_coords(90.01,  0.0));
        assert!(!is_valid_coords(-90.01, 0.0));
        assert!(!is_valid_coords(0.0,    180.01));
        assert!(!is_valid_coords(0.0,   -180.01));
    }

    #[test]
    fn coord_validation_rejects_non_finite() {
        assert!(!is_valid_coords(f64::NAN,       0.0));
        assert!(!is_valid_coords(0.0,            f64::NAN));
        assert!(!is_valid_coords(f64::INFINITY,  0.0));
        assert!(!is_valid_coords(0.0,            f64::NEG_INFINITY));
    }

    // ── Guess recording ───────────────────────────────────────────────────────

    fn make_game() -> ActiveGame {
        ActiveGame {
            event_id: "$test:example.com".try_into().unwrap(),
            mode: ActiveGameMode::FreeGuess {
                guesses:    HashMap::new(),
                actual_lat: 48.13,
                actual_lon: 11.58,
            },
        }
    }

    fn make_guess(lat: f64, lon: f64) -> FreeGuess {
        FreeGuess { text: format!("{lat},{lon}"), lat, lon, submitted_at: chrono::Utc::now() }
    }

    fn guesses_of(ag: &ActiveGame) -> &HashMap<String, FreeGuess> {
        let ActiveGameMode::FreeGuess { ref guesses, .. } = ag.mode;
        guesses
    }

    #[test]
    fn submit_records_guess() {
        let mut ag = make_game();
        let uid = "@alice:example.com".to_owned();
        let accepted = ag.record_free_guess(uid.clone(), make_guess(10.0, 20.0), 0);
        assert!(accepted, "first submission should be accepted");
        let guesses = guesses_of(&ag);
        assert!(guesses.contains_key(&uid), "guess must be stored");
        assert_eq!(guesses[&uid].lat, 10.0);
    }

    #[test]
    fn one_shot_blocks_second_submission() {
        let mut ag = make_game();
        let uid = "@alice:example.com".to_owned();
        let first  = ag.record_free_guess(uid.clone(), make_guess(10.0, 20.0), 1);
        let second = ag.record_free_guess(uid.clone(), make_guess(30.0, 40.0), 1);
        assert!(first,  "first submission must be accepted in one-shot mode");
        assert!(!second, "second submission must be rejected in one-shot mode");
        assert_eq!(guesses_of(&ag)[&uid].lat, 10.0, "original guess must not be overwritten");
    }

    #[test]
    fn unlimited_mode_allows_update() {
        let mut ag = make_game();
        let uid = "@alice:example.com".to_owned();
        ag.record_free_guess(uid.clone(), make_guess(10.0, 20.0), 0);
        let updated = ag.record_free_guess(uid.clone(), make_guess(30.0, 40.0), 0);
        assert!(updated, "update must be accepted in unlimited mode");
        assert_eq!(guesses_of(&ag)[&uid].lat, 30.0, "guess must be updated to latest value");
    }

    #[test]
    fn multiplayer_all_submit_independently() {
        let mut ag = make_game();
        let players = ["@alice:s", "@bob:s", "@carol:s"];
        for (i, uid) in players.iter().enumerate() {
            let accepted = ag.record_free_guess(uid.to_string(), make_guess(i as f64, i as f64), 0);
            assert!(accepted, "{uid} should be accepted");
        }
        assert_eq!(guesses_of(&ag).len(), 3, "all 3 guesses must be recorded");
    }

    #[test]
    fn authorization_wrong_token_not_found() {
        let store = TokenStore::default();
        assert!(store.tokens.get("nonexistent").is_none(), "unknown token must not be found");
    }
}

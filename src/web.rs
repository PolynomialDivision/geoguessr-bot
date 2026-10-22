//! HTTP server for interactive map-based guess submission.
//!
//! Each participant gets a personal token URL, posted in the game room.
//! Opening the URL shows a MapLibre world map (the library served from here,
//! assets/maplibre-gl-4.7.1).  Clicking places a draggable pin; pressing
//! Submit records the guess.  Tokens expire at round end.

use mxbot_common::format::html_escape;
use mxbot_common::matrix_sdk;
use mxbot_common::send::send_with_retry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use matrix_sdk::{
    ruma::{OwnedEventId, OwnedRoomId, OwnedUserId},
    Client,
};
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, sync::Mutex};
use tracing::{info, warn};

use crate::game::{ActiveGame, ActiveGameMode, FreeGuess};

// ── Token + session store ─────────────────────────────────────────────────────

/// One record per player per guess.
#[derive(Clone, Debug)]
pub struct GuessToken {
    pub user_id: OwnedUserId,
    pub round_id: i64,
    pub guess_num: u32,
    pub lang: String,
}

/// Per-guess session shared across all tokens for the same (round_id, guess_num).
#[derive(Clone, Debug)]
pub struct GuessSession {
    /// Event ID of the links message in the game room: edited on each submission.
    pub links_event_id: OwnedEventId,
    /// Ordered participant list (controls display order in status line).
    pub participants: Vec<OwnedUserId>,
}

#[derive(Default)]
pub struct TokenStore {
    pub tokens: HashMap<String, GuessToken>,
    pub sessions: HashMap<(i64, u32), GuessSession>,
}

pub type SharedTokenStore = Arc<Mutex<TokenStore>>;

pub fn new_token_store() -> SharedTokenStore {
    Arc::new(Mutex::new(TokenStore::default()))
}

// ── Axum shared state ─────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct WebState {
    pub store: SharedTokenStore,
    pub active_game: Arc<Mutex<Option<ActiveGame>>>,
    /// The state file: every accepted guess is written there so a restart keeps it.
    pub state: Arc<Mutex<crate::state::State>>,
    pub state_path: std::path::PathBuf,
    pub client: Client,
    pub room_id: OwnedRoomId,
    /// 0 = unlimited updates; >0 = only the first submission counts.
    pub max_guesses: u32,
    /// Public base URL (no trailing slash).
    pub public_url: String,
}

// ── Server entry point ────────────────────────────────────────────────────────

pub async fn run(bind_addr: String, state: WebState) -> anyhow::Result<()> {
    let app = router(state);

    let listener = TcpListener::bind(&bind_addr).await?;
    info!("Geo-guess web server listening on {bind_addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

pub fn router(state: WebState) -> Router {
    asset_routes(Router::new())
        .route("/g/:token", get(serve_map))
        .route("/g/:token/submit", post(submit_guess))
        .route("/map-test", get(test_map))
        .with_state(state)
}

// ── Map page ──────────────────────────────────────────────────────────────────

async fn serve_map(
    Path(token): Path<String>,
    State(ws): State<WebState>,
    headers: HeaderMap,
) -> Response {
    let info = {
        let store = ws.store.lock().await;
        store.tokens.get(&token).cloned()
    };
    let Some(tok) = info else {
        return page(StatusCode::NOT_FOUND, expired_html(reader_lang(&headers)));
    };
    let one_shot = ws.max_guesses > 0;
    page(
        StatusCode::OK,
        map_html(&token, tok.user_id.localpart(), one_shot, &tok.lang),
    )
}

async fn test_map(Query(params): Query<HashMap<String, String>>) -> Response {
    let lang = params
        .get("lang")
        .cloned()
        .unwrap_or_else(|| "en".to_owned());
    page(StatusCode::OK, map_html("test", "test", false, &lang))
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
    State(ws): State<WebState>,
    Json(body): Json<SubmitBody>,
) -> Response {
    if !is_valid_coords(body.lat, body.lon) {
        return (StatusCode::BAD_REQUEST, "Invalid coordinates").into_response();
    }

    let tok = {
        let store = ws.store.lock().await;
        match store.tokens.get(&token).cloned() {
            Some(t) => t,
            None => return (StatusCode::NOT_FOUND, "Link expired or invalid").into_response(),
        }
    };

    let already_submitted = {
        let ag = ws.active_game.lock().await;
        ag.as_ref().is_some_and(|g| {
            let ActiveGameMode::FreeGuess { ref guesses, .. } = g.mode;
            guesses.contains_key(tok.user_id.as_str())
        })
    };

    // One-shot: acknowledge silently but don't overwrite the original guess.
    if already_submitted && ws.max_guesses > 0 {
        let geocoded = crate::geocode::reverse_geocode(body.lat, body.lon, &tok.lang).await;
        return (StatusCode::OK, Json(SubmitResponse { geocoded })).into_response();
    }

    // None = this guess has already closed.
    let accepted = {
        let mut ag = ws.active_game.lock().await;
        ag.as_mut().map(|g| {
            g.record_free_guess(
                tok.user_id.as_str().to_owned(),
                FreeGuess {
                    text: format!("{:.4},{:.4}", body.lat, body.lon),
                    lat: body.lat,
                    lon: body.lon,
                    submitted_at: chrono::Utc::now(),
                },
                ws.max_guesses,
            )
        })
    };

    info!(
        "Web guess: {} → ({:.4}, {:.4}) accepted={:?}",
        tok.user_id, body.lat, body.lon, accepted
    );

    match accepted {
        Some(true) => {
            crate::game::persist_guesses(&ws.state, &ws.state_path, &ws.active_game).await;
            update_links_message(&ws, tok.round_id, tok.guess_num).await;
        }
        Some(false) => {}
        // The page shows this text instead of a "recorded" confirmation that
        // would never count.
        None => return (StatusCode::GONE, "⏰ Time is up. This guess has closed.").into_response(),
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
            None => return,
        };
        let token_by_user: HashMap<String, String> = store
            .tokens
            .iter()
            .filter(|(_, t)| t.round_id == round_id && t.guess_num == guess_num)
            .map(|(tok, t)| (t.user_id.to_string(), tok.clone()))
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

    let line = session
        .participants
        .iter()
        .map(|uid| {
            if submitted.contains(uid.as_str()) {
                format!("✅ {}", uid.localpart())
            } else if let Some(tok) = token_by_user.get(uid.as_str()) {
                format!("[🗺️ {}]({}/g/{})", uid.localpart(), ws.public_url, tok)
            } else {
                format!("⏳ {}", uid.localpart())
            }
        })
        .collect::<Vec<_>>()
        .join("  ·  ");

    let Some(room) = ws.client.get_room(&ws.room_id) else {
        return;
    };
    use matrix_sdk::ruma::events::room::message::ReplacementMetadata;
    let edit = crate::format::mentionify(&line).make_replacement(ReplacementMetadata::new(
        session.links_event_id.clone(),
        None,
    ));
    if let Err(e) = send_with_retry(&room, edit).await {
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

// ── Coordinate validation ─────────────────────────────────────────────────────

pub fn is_valid_coords(lat: f64, lon: f64) -> bool {
    lat.is_finite() && lon.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0
}

// ── HTML ──────────────────────────────────────────────────────────────────────

/// MapLibre GL JS, served from the bot (assets/maplibre-gl-4.7.1/SOURCE.txt): no CDN, so
/// no third party can change the script the page runs.
const MAPLIBRE_JS: &str = include_str!("../assets/maplibre-gl-4.7.1/maplibre-gl.js");
const MAPLIBRE_CSS: &str = include_str!("../assets/maplibre-gl-4.7.1/maplibre-gl.css");
const MAP_JS: &str = include_str!("../assets/web/map.js");
const MAP_CSS: &str = include_str!("../assets/web/map.css");

/// The page's policy: scripts and styles only from here; the map's tiles, glyphs and
/// sprites from OpenFreeMap, the satellite tiles from Esri, the place search at Nominatim.
/// MapLibre starts its worker from a blob: URL and draws images from blob: and data: URLs.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
img-src 'self' data: blob: https://tiles.openfreemap.org https://server.arcgisonline.com; \
connect-src 'self' https://tiles.openfreemap.org https://server.arcgisonline.com https://nominatim.openstreetmap.org; \
font-src 'self'; worker-src blob:; child-src blob:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

fn page(status: StatusCode, html: String) -> Response {
    let mut r = (status, Html(html)).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    r
}

/// A static file. The MapLibre files carry their version in the path and never change;
/// the page's own script and style are kept for an hour.
fn asset(kind: &'static str, body: &'static str, immutable: bool) -> Response {
    let cache = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, cache),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

pub fn asset_routes<S: Clone + Send + Sync + 'static>(router: Router<S>) -> Router<S> {
    router
        .route(
            "/assets/maplibre-gl-4.7.1/maplibre-gl.js",
            get(|| async { asset("text/javascript; charset=utf-8", MAPLIBRE_JS, true) }),
        )
        .route(
            "/assets/maplibre-gl-4.7.1/maplibre-gl.css",
            get(|| async { asset("text/css; charset=utf-8", MAPLIBRE_CSS, true) }),
        )
        .route(
            "/assets/map.js",
            get(|| async { asset("text/javascript; charset=utf-8", MAP_JS, false) }),
        )
        .route(
            "/assets/map.css",
            get(|| async { asset("text/css; charset=utf-8", MAP_CSS, false) }),
        )
}

/// A language code for `<html lang>`: letters, digits and dashes, else "en".
fn page_lang(lang: &str) -> &str {
    let ok = (1..=12).contains(&lang.len())
        && lang.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && lang.starts_with(|c: char| c.is_ascii_alphabetic());
    if ok {
        lang
    } else {
        "en"
    }
}

/// "rtl" for the right-to-left languages players can pick (Arabic, Hebrew).
fn text_dir(lang: &str) -> &'static str {
    match lang.split('-').next().unwrap_or("") {
        "ar" | "he" => "rtl",
        _ => "ltr",
    }
}

/// The map page. The values reach the script as escaped data attributes (assets/web/map.js
/// reads them), so the page has no inline script.
fn map_html(token: &str, display_name: &str, one_shot: bool, lang: &str) -> String {
    let lang = page_lang(lang);
    MAP_PAGE_TEMPLATE
        .replace("__LANG__", lang)
        .replace("__DIR__", text_dir(lang))
        .replace("__TOKEN__", &html_escape(token))
        .replace("__NAME__", &html_escape(display_name))
        .replace("__ONE_SHOT__", if one_shot { "true" } else { "false" })
}

static MAP_PAGE_TEMPLATE: &str = r#"<!DOCTYPE html>
<html lang="__LANG__" dir="__DIR__">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>GeoGuessr</title>
<link rel="stylesheet" href="/assets/maplibre-gl-4.7.1/maplibre-gl.css">
<link rel="stylesheet" href="/assets/map.css">
</head>
<body data-token="__TOKEN__" data-name="__NAME__" data-one-shot="__ONE_SHOT__" data-lang="__LANG__">
<div id="map"></div>
<div id="search">
  <input id="search-input" type="search" placeholder="Search a place or paste lat, lon…" aria-label="Search a place or paste lat, lon" autocomplete="off">
  <button id="search-btn" type="button" aria-label="Search">🔍</button>
</div>
<button id="layer-toggle" type="button" aria-label="Show satellite view">🛰 Satellite</button>
<div id="bar">
  <span id="hint" aria-live="polite">Tap the map to place your pin</span>
  <button id="btn" type="button" disabled>Submit guess</button>
  <span id="name"></span>
</div>
<script src="/assets/maplibre-gl-4.7.1/maplibre-gl.js"></script>
<script src="/assets/map.js"></script>
</body>
</html>"#;

/// The page's texts for a link that no longer works: (heading, why, what now).
fn expired_texts(lang: &str) -> (&'static str, &'static str, &'static str) {
    match lang {
        "de" => (
            "⏰ Dieser Link ist abgelaufen",
            "Die Runde ist vorbei oder der Link stimmt nicht.",
            "💬 Zurück in den Chat: Mit der nächsten Runde kommt ein neuer Link.",
        ),
        "uk" => (
            "⏰ Термін дії посилання минув",
            "Раунд закінчився або посилання неправильне.",
            "💬 Повертайтеся в чат: наступний раунд принесе нове посилання.",
        ),
        _ => (
            "⏰ This link has expired",
            "The round has ended or the link is wrong.",
            "💬 Back to the chat: the next round brings a new link.",
        ),
    }
}

/// The reader's language for pages without a token: the house's `house_lang` cookie
/// (set by the start page), else the first of German, English and Ukrainian in
/// Accept-Language, else English.
pub fn reader_lang(headers: &HeaderMap) -> &'static str {
    const KNOWN: [&str; 3] = ["de", "en", "uk"];
    let pick = |code: &str| {
        let base = code.trim().split(['-', '_']).next().unwrap_or("");
        KNOWN.iter().find(|k| k.eq_ignore_ascii_case(base)).copied()
    };
    let cookie = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().strip_prefix("house_lang="))
        .find_map(pick);
    if let Some(lang) = cookie {
        return lang;
    }
    headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .into_iter()
        .flat_map(|v| v.split(','))
        .filter(|part| {
            // "de;q=0" means "not German".
            part.split(';').skip(1).all(|param| {
                param
                    .trim()
                    .strip_prefix("q=")
                    .and_then(|q| q.parse::<f32>().ok())
                    .is_none_or(|q| q > 0.0)
            })
        })
        .find_map(|part| pick(part.split(';').next().unwrap_or("")))
        .unwrap_or("en")
}

fn expired_html(lang: &str) -> String {
    let (heading, why, next) = expired_texts(lang);
    format!(
        r#"<!DOCTYPE html>
<html lang="{lang}">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{heading}</title>
<link rel="stylesheet" href="/assets/map.css">
</head>
<body class="expired"><main>
<h1>{heading}</h1>
<p>{why}</p>
<p>{next}</p>
</main></body></html>"#
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{ActiveGame, ActiveGameMode};
    use matrix_sdk::ruma::OwnedUserId;
    use std::collections::HashMap;

    // ── Pages ─────────────────────────────────────────────────────────────────

    #[test]
    fn page_values_are_escaped_attributes_and_the_page_has_no_inline_script() {
        let html = map_html("tok", "\"><script>alert(1)</script>&", false, "de");
        assert!(!html.contains("<script>alert"), "{html}");
        assert!(
            html.contains(r#"data-name="&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;&amp;""#)
        );
        // Every script comes from this server (CSP script-src 'self').
        assert_eq!(html.matches("<script").count(), 2);
        assert_eq!(html.matches("<script src=\"/assets/").count(), 2);
        assert!(!html.contains("unpkg") && !html.contains("https://"));
    }

    #[test]
    fn the_page_language_and_direction() {
        let html = map_html("tok", "a", true, "uk");
        assert!(html.contains(r#"<html lang="uk" dir="ltr">"#));
        assert!(html.contains(r#"data-one-shot="true""#));
        assert!(map_html("t", "a", false, "ar").contains(r#"<html lang="ar" dir="rtl">"#));
        assert!(map_html("t", "a", false, "he").contains(r#"dir="rtl""#));
        // /map-test?lang=… can't break out of the attribute.
        let html = map_html("t", "a", false, "x\" onload=\"alert(1)");
        assert!(html.contains(r#"<html lang="en" dir="ltr">"#), "{html}");
        assert!(!html.contains("onload"));
    }

    #[test]
    fn the_search_and_map_buttons_have_names() {
        let html = map_html("t", "a", false, "en");
        assert!(html.contains(r#"id="search-input" type="search""#));
        for id in ["search-input", "search-btn", "layer-toggle"] {
            let at = html.find(&format!("id=\"{id}\"")).unwrap();
            let tag = &html[at..at + html[at..].find('>').unwrap()];
            assert!(tag.contains("aria-label=\""), "{id}: {tag}");
        }
    }

    #[test]
    fn the_vendored_maplibre_is_the_pinned_version() {
        assert!(MAPLIBRE_JS.starts_with("/**\n * MapLibre GL JS"));
        assert!(MAPLIBRE_JS[..200].contains("/v4.7.1/LICENSE.txt"));
        assert!(MAPLIBRE_CSS.contains(".maplibregl-map"));
        // Without eval: script-src needs no 'unsafe-eval'.
        assert!(!MAPLIBRE_JS.contains("new Function"));
    }

    #[test]
    fn the_policy_allows_what_the_map_loads_and_nothing_inline() {
        assert!(CSP.contains("script-src 'self';"));
        assert!(CSP.contains("style-src 'self';"));
        assert!(!CSP.contains("unsafe"));
        for host in [
            "https://tiles.openfreemap.org",
            "https://server.arcgisonline.com",
            "https://nominatim.openstreetmap.org",
        ] {
            assert!(CSP.contains(host), "{host}");
            assert!(MAP_JS.contains(host), "{host} is no longer used: drop it");
        }
        assert!(CSP.contains("worker-src blob:"));
    }

    fn lang_of(pairs: &[(header::HeaderName, &str)]) -> &'static str {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(name.clone(), HeaderValue::from_str(value).unwrap());
        }
        reader_lang(&headers)
    }

    #[test]
    fn the_expired_page_speaks_the_readers_language() {
        assert_eq!(lang_of(&[]), "en");
        assert_eq!(
            lang_of(&[(header::ACCEPT_LANGUAGE, "de-DE,de;q=0.9,en;q=0.8")]),
            "de"
        );
        assert_eq!(
            lang_of(&[(header::ACCEPT_LANGUAGE, "fr-FR, uk;q=0.5")]),
            "uk"
        );
        assert_eq!(lang_of(&[(header::ACCEPT_LANGUAGE, "de;q=0, en")]), "en");
        // The start page's choice wins.
        assert_eq!(
            lang_of(&[
                (header::COOKIE, "a=b; house_lang=uk"),
                (header::ACCEPT_LANGUAGE, "de")
            ]),
            "uk"
        );
        let html = expired_html("de");
        assert!(html.contains(r#"<html lang="de">"#));
        assert!(html.contains("abgelaufen") && html.contains("Chat"));
        assert!(expired_html("en").contains("Back to the chat"));
        assert!(expired_html("uk").contains("чат"));
    }

    #[tokio::test]
    async fn assets_are_served_with_their_types() {
        use tower::ServiceExt;
        let app: Router = asset_routes(Router::new());
        for (path, kind) in [
            (
                "/assets/maplibre-gl-4.7.1/maplibre-gl.js",
                "text/javascript",
            ),
            ("/assets/maplibre-gl-4.7.1/maplibre-gl.css", "text/css"),
            ("/assets/map.js", "text/javascript"),
            ("/assets/map.css", "text/css"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::get(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let ct = response.headers()[header::CONTENT_TYPE].to_str().unwrap();
            assert!(ct.starts_with(kind), "{path}: {ct}");
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
        }
    }

    // ── Token generation ──────────────────────────────────────────────────────

    #[test]
    fn token_is_24_alphanumeric_chars() {
        let t = generate_token();
        assert_eq!(t.len(), 24, "token must be 24 chars");
        assert!(
            t.chars().all(|c| c.is_ascii_alphanumeric()),
            "token must be alphanumeric"
        );
    }

    #[test]
    fn tokens_are_unique() {
        let tokens: Vec<String> = (0..200).map(|_| generate_token()).collect();
        let set: HashSet<&String> = tokens.iter().collect();
        assert_eq!(
            set.len(),
            tokens.len(),
            "all 200 generated tokens must be distinct"
        );
    }

    // ── Token store ───────────────────────────────────────────────────────────

    #[test]
    fn token_store_add_and_retrieve() {
        let uid: OwnedUserId = "@alice:example.com".try_into().unwrap();
        let mut store = TokenStore::default();
        store.tokens.insert(
            "tok1".to_owned(),
            GuessToken {
                user_id: uid.clone(),
                round_id: 1,
                guess_num: 1,
                lang: "en".to_owned(),
            },
        );
        let found = store.tokens.get("tok1");
        assert!(found.is_some());
        assert_eq!(found.unwrap().user_id, uid);
        assert!(!store.tokens.contains_key("wrong_token"));
    }

    #[test]
    fn token_store_clear_by_round_and_guess() {
        let uid: OwnedUserId = "@alice:example.com".try_into().unwrap();
        let mut store = TokenStore::default();
        store.tokens.insert(
            "t_r1g1".to_owned(),
            GuessToken {
                user_id: uid.clone(),
                round_id: 1,
                guess_num: 1,
                lang: "en".to_owned(),
            },
        );
        store.tokens.insert(
            "t_r1g2".to_owned(),
            GuessToken {
                user_id: uid.clone(),
                round_id: 1,
                guess_num: 2,
                lang: "en".to_owned(),
            },
        );
        store.tokens.insert(
            "t_r2g1".to_owned(),
            GuessToken {
                user_id: uid.clone(),
                round_id: 2,
                guess_num: 1,
                lang: "en".to_owned(),
            },
        );

        // Simulate clearing round 1, guess 1.
        store
            .tokens
            .retain(|_, t| !(t.round_id == 1 && t.guess_num == 1));
        store.sessions.remove(&(1, 1));

        assert!(
            !store.tokens.contains_key("t_r1g1"),
            "cleared token must be gone"
        );
        assert!(
            store.tokens.contains_key("t_r1g2"),
            "other guess token must survive"
        );
        assert!(
            store.tokens.contains_key("t_r2g1"),
            "other round token must survive"
        );
    }

    #[test]
    fn token_store_full_clear() {
        let uid: OwnedUserId = "@alice:example.com".try_into().unwrap();
        let mut store = TokenStore::default();
        for i in 0..5 {
            store.tokens.insert(
                format!("tok{i}"),
                GuessToken {
                    user_id: uid.clone(),
                    round_id: 1,
                    guess_num: i,
                    lang: "en".to_owned(),
                },
            );
        }
        store.tokens.clear();
        store.sessions.clear();
        assert!(store.tokens.is_empty());
        assert!(store.sessions.is_empty());
    }

    // ── Coordinate validation ─────────────────────────────────────────────────

    #[test]
    fn coord_validation_accepts_valid() {
        assert!(is_valid_coords(0.0, 0.0));
        assert!(is_valid_coords(90.0, 180.0));
        assert!(is_valid_coords(-90.0, -180.0));
        assert!(is_valid_coords(48.13, 11.58));
        assert!(is_valid_coords(-33.87, 151.21));
    }

    #[test]
    fn coord_validation_rejects_out_of_range() {
        assert!(!is_valid_coords(90.01, 0.0));
        assert!(!is_valid_coords(-90.01, 0.0));
        assert!(!is_valid_coords(0.0, 180.01));
        assert!(!is_valid_coords(0.0, -180.01));
    }

    #[test]
    fn coord_validation_rejects_non_finite() {
        assert!(!is_valid_coords(f64::NAN, 0.0));
        assert!(!is_valid_coords(0.0, f64::NAN));
        assert!(!is_valid_coords(f64::INFINITY, 0.0));
        assert!(!is_valid_coords(0.0, f64::NEG_INFINITY));
    }

    // ── Guess recording ───────────────────────────────────────────────────────

    fn make_game() -> ActiveGame {
        ActiveGame {
            event_id: "$test:example.com".try_into().unwrap(),
            mode: ActiveGameMode::FreeGuess {
                guesses: HashMap::new(),
                actual_lat: 48.13,
                actual_lon: 11.58,
            },
        }
    }

    fn make_guess(lat: f64, lon: f64) -> FreeGuess {
        FreeGuess {
            text: format!("{lat},{lon}"),
            lat,
            lon,
            submitted_at: chrono::Utc::now(),
        }
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
        let first = ag.record_free_guess(uid.clone(), make_guess(10.0, 20.0), 1);
        let second = ag.record_free_guess(uid.clone(), make_guess(30.0, 40.0), 1);
        assert!(first, "first submission must be accepted in one-shot mode");
        assert!(
            !second,
            "second submission must be rejected in one-shot mode"
        );
        assert_eq!(
            guesses_of(&ag)[&uid].lat,
            10.0,
            "original guess must not be overwritten"
        );
    }

    #[test]
    fn unlimited_mode_allows_update() {
        let mut ag = make_game();
        let uid = "@alice:example.com".to_owned();
        ag.record_free_guess(uid.clone(), make_guess(10.0, 20.0), 0);
        let updated = ag.record_free_guess(uid.clone(), make_guess(30.0, 40.0), 0);
        assert!(updated, "update must be accepted in unlimited mode");
        assert_eq!(
            guesses_of(&ag)[&uid].lat,
            30.0,
            "guess must be updated to latest value"
        );
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
        assert!(
            !store.tokens.contains_key("nonexistent"),
            "unknown token must not be found"
        );
    }
}

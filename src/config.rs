pub use mxbot_common::config::{MatrixConfig, SecurityConfig};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    pub matrix: MatrixConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    pub schedule: ScheduleConfig,
    pub sources: SourcesConfig,
    #[serde(default)]
    pub web: Option<WebConfig>,
    #[serde(default)]
    pub map: MapConfig,
}

/// Tiles for the result map images.
///
/// ```toml
/// [map]
/// tile_url    = "https://tile.openstreetmap.org/{z}/{x}/{y}.png"
/// attribution = "© OpenStreetMap contributors"
/// ```
#[derive(Deserialize, Clone)]
pub struct MapConfig {
    /// XYZ tile URL template with `{z}`, `{x}`, `{y}`.
    #[serde(default = "default_tile_url")]
    pub tile_url: String,
    /// Credit printed in the image corner, as the tile provider requires.
    #[serde(default = "default_attribution")]
    pub attribution: String,
}

impl Default for MapConfig {
    fn default() -> Self {
        Self {
            tile_url: default_tile_url(),
            attribution: default_attribution(),
        }
    }
}

fn default_tile_url() -> String {
    "https://tile.openstreetmap.org/{z}/{x}/{y}.png".to_owned()
}

fn default_attribution() -> String {
    "© OpenStreetMap contributors".to_owned()
}

/// Optional HTTP server for the interactive map-based guess interface.
///
/// ```toml
/// [web]
/// bind_addr  = "0.0.0.0:8080"
/// public_url = "https://geo.example.com"
/// ```
#[derive(Deserialize, Clone)]
pub struct WebConfig {
    /// Address the bot listens on (e.g. "0.0.0.0:8080").
    pub bind_addr: String,
    /// Public base URL players open in their browser (no trailing slash).
    /// Overridden by the environment variable [`PUBLIC_URL_ENV`] if it is set.
    pub public_url: String,
}

/// Environment variable that overrides `[web] public_url`, so the deployment can
/// set the public address (e.g. after a domain change) without editing the config.
pub const PUBLIC_URL_ENV: &str = "GEOGUESSR_PUBLIC_URL";

impl WebConfig {
    /// Uses `value` as `public_url` if it is set and not blank. A trailing slash is
    /// dropped, because links are built as `{public_url}/g/{token}`.
    pub fn apply_public_url_override(&mut self, value: Option<String>) {
        if let Some(url) = value.map(|v| v.trim().trim_end_matches('/').to_owned()) {
            if !url.is_empty() {
                self.public_url = url;
            }
        }
    }
}

#[derive(Deserialize)]
pub struct ScheduleConfig {
    pub room_id: String,
    /// One or more "HH:MM" times (in the configured timezone) to run the game.
    pub game_times: Vec<String>,
    /// Seconds to collect answers per guess before revealing.
    #[serde(default = "default_answer_timeout")]
    pub answer_timeout_secs: u64,
    /// Number of guesses (locations) per round.
    #[serde(default = "default_guesses_per_round", alias = "images_per_round")]
    pub guesses_per_round: u32,
    /// Pause in seconds between pictures.
    #[serde(default = "default_inter_guess_secs", alias = "inter_image_secs")]
    pub inter_guess_secs: u64,
    /// Seconds before game_time to post a "starting soon" reminder. 0 = disabled.
    #[serde(default = "default_reminder_before_secs")]
    pub reminder_before_secs: u64,
    /// IANA timezone (e.g. "Europe/Berlin").
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// Emoji the bot reacts with on the join-prompt message.
    /// Any other user who reacts with the same emoji opts in to play via DM.
    /// Only used when game_mode = "free_guess" and reminder_before_secs > 0.
    #[serde(default = "default_join_emoji")]
    pub join_emoji: String,
    /// How many photos to post for a single guess location.
    /// Default 1 (classic single-photo). Increase to give players more visual context.
    #[serde(default = "default_photos_per_location")]
    pub photos_per_location: usize,
    /// Maximum guesses a player may submit per image.
    /// 0 (default) = unlimited. Each new guess overwrites the previous one.
    /// 1 = only the first guess counts; subsequent attempts are rejected.
    #[serde(default)]
    pub max_guesses_per_player: u32,
    /// Half-life distance for the scoring curve: score = 5000 × e^(−dist_km / half_life).
    /// Default 2000 km (original GeoGuessr-style). Lower values reward precision more:
    ///   1000 = 50 km scores ~4753 pts instead of 4876.
    ///   500  = 50 km scores ~4512 pts.
    #[serde(default = "default_score_half_life_km")]
    pub score_half_life_km: f64,
    /// Leaderboard tuning, see `LeaderboardConfig`. An old `[schedule.rating]`
    /// table from the former rating is ignored.
    #[serde(default)]
    pub leaderboard: LeaderboardConfig,
}

impl ScheduleConfig {
    pub fn parse_game_time(s: &str) -> Option<(u32, u32)> {
        let (h, m) = s.split_once(':')?;
        let hour: u32 = h.trim().parse().ok()?;
        let minute: u32 = m.trim().parse().ok()?;
        if hour < 24 && minute < 60 {
            Some((hour, minute))
        } else {
            None
        }
    }
}

fn default_join_emoji() -> String {
    "👍".to_owned()
}
fn default_answer_timeout() -> u64 {
    90
}
fn default_guesses_per_round() -> u32 {
    5
}
fn default_inter_guess_secs() -> u64 {
    15
}
fn default_reminder_before_secs() -> u64 {
    300
}
fn default_timezone() -> String {
    "UTC".to_owned()
}
fn default_photos_per_location() -> usize {
    1
}
fn default_score_half_life_km() -> f64 {
    2000.0
}

/// Leaderboards rank by duels, see `duels`: the month by duel points, the
/// 90-day and all-time boards by the share of duels won.
#[derive(Deserialize, Clone, Copy)]
pub struct LeaderboardConfig {
    /// Images a player needs before the 90-day and all-time boards rank them
    /// by their share of duels won. Below it they are listed apart, so two
    /// lucky images can't top a board built over months.
    #[serde(default = "default_min_images")]
    pub min_images: i64,
}

impl Default for LeaderboardConfig {
    fn default() -> Self {
        Self {
            min_images: default_min_images(),
        }
    }
}

fn default_min_images() -> i64 {
    20
}

// ── Sources ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SourcesConfig {
    /// Which sources to draw images from: "mapillary", "local".
    /// The bot picks a random source from this list for each image.
    #[serde(default = "default_enabled_sources")]
    pub enabled: Vec<String>,

    #[serde(default)]
    pub mapillary: MapillaryConfig,

    #[serde(default)]
    pub local: LocalConfig,
}

fn default_enabled_sources() -> Vec<String> {
    vec!["mapillary".to_owned()]
}

#[derive(Deserialize, Default)]
pub struct MapillaryConfig {
    /// Mapillary client access token (required).
    /// Get one free at https://www.mapillary.com/developer
    #[serde(default)]
    pub access_token: String,
    /// Desired search radius in metres around the seed coordinate.
    /// We search via Mapillary's `bbox` query (not `radius`: that param is
    /// capped by the API at 50 *metres*, far too small to find enough
    /// images). `bbox` has its own hard cap of "smaller than 0.01 degrees
    /// square", which works out to roughly 500 m–1.1 km per side depending
    /// on latitude. Any value here larger than that ceiling is silently
    /// clamped down to it. There is no way to search a wider area via this
    /// endpoint. See `sources::mapillary::seed_bbox`.
    #[serde(default = "default_mapillary_radius")]
    pub search_radius: u32,
    /// Optional ISO 3166-1 alpha-2 country filter (same as wikimedia).
    #[serde(default)]
    pub countries: Vec<String>,
}

fn default_mapillary_radius() -> u32 {
    50_000
}

#[derive(Deserialize, Default)]
pub struct LocalConfig {
    /// Directory that contains images and an `index.json` file.
    /// index.json format: array of { file, country, region, city?, attribution? }
    pub path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web() -> WebConfig {
        WebConfig {
            bind_addr: "0.0.0.0:80".to_owned(),
            public_url: "https://old.example.net".to_owned(),
        }
    }

    #[test]
    fn public_url_override_replaces_the_config_value() {
        let mut w = web();
        w.apply_public_url_override(Some("https://geo.example.org/".to_owned()));
        assert_eq!(w.public_url, "https://geo.example.org");
    }

    #[test]
    fn missing_or_blank_override_keeps_the_config_value() {
        let mut w = web();
        w.apply_public_url_override(None);
        assert_eq!(w.public_url, "https://old.example.net");
        w.apply_public_url_override(Some("  ".to_owned()));
        assert_eq!(w.public_url, "https://old.example.net");
    }
}

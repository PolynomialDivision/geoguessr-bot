pub use mxbot_common::config::{EncryptionStrategy, MatrixConfig, VerificationConfig};
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
    pub public_url: String,
}

#[derive(Deserialize, Default)]
pub struct SecurityConfig {
    #[serde(default)]
    pub allowed_inviters: Vec<String>,
    #[serde(default)]
    pub admin_users: Vec<String>,
    #[serde(default)]
    pub encryption_strategy: EncryptionStrategy,
    #[serde(default)]
    pub verification: VerificationConfig,
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
    /// 0 (default) = unlimited — each new guess overwrites the previous one.
    /// 1 = only the first guess counts; subsequent attempts are rejected.
    #[serde(default)]
    pub max_guesses_per_player: u32,
    /// Half-life distance for the scoring curve: score = 5000 × e^(−dist_km / half_life).
    /// Default 2000 km (original GeoGuessr-style). Lower values reward precision more:
    ///   1000 = 50 km scores ~4753 pts instead of 4876.
    ///   500  = 50 km scores ~4512 pts.
    #[serde(default = "default_score_half_life_km")]
    pub score_half_life_km: f64,
    /// Leaderboard rating tuning — see `RatingConfig`.
    #[serde(default)]
    pub rating: RatingConfig,
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

/// Tuning for the leaderboard's Bayesian-shrinkage rating:
///
///   rating = (n / (n + k)) * player_average + (k / (n + k)) * baseline
///
/// where `n` is a player's guesses played (real submissions plus missed
/// guesses, which count as 0 — see `Db::record_missed_guesses`), and
/// `baseline` is the community's own average score per guess (derived from
/// the leaderboard data itself, not configured here — see
/// `commands::community_baseline`).
#[derive(Deserialize, Clone, Copy)]
pub struct RatingConfig {
    /// How many "prior" guesses worth of pull toward the baseline a
    /// player's rating gets. Higher = trusts a small sample less (rating
    /// stays closer to baseline for longer); lower = trusts it sooner.
    /// At n = k, a player's rating is exactly halfway between their own
    /// average and the baseline.
    #[serde(default = "default_rating_k")]
    pub k: f64,
    /// Guesses played below which a leaderboard entry is flagged
    /// "provisional" — not yet enough data to be a confident read on skill.
    /// Purely a display hint; does not change the rating math itself.
    #[serde(default = "default_provisional_threshold")]
    pub provisional_threshold: i64,
    /// Rating baseline used only when there is no community data yet to
    /// derive one from (e.g. right after `!resetstats`). Once any guesses
    /// exist, the real community average is used instead.
    #[serde(default = "default_baseline_fallback")]
    pub baseline_fallback: f64,
}

impl Default for RatingConfig {
    fn default() -> Self {
        Self {
            k: default_rating_k(),
            provisional_threshold: default_provisional_threshold(),
            baseline_fallback: default_baseline_fallback(),
        }
    }
}

fn default_rating_k() -> f64 {
    15.0
}
fn default_provisional_threshold() -> i64 {
    10
}
fn default_baseline_fallback() -> f64 {
    1500.0
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
    /// We search via Mapillary's `bbox` query (not `radius` — that param is
    /// capped by the API at 50 *metres*, far too small to find enough
    /// images). `bbox` has its own hard cap of "smaller than 0.01 degrees
    /// square", which works out to roughly 500 m–1.1 km per side depending
    /// on latitude. Any value here larger than that ceiling is silently
    /// clamped down to it — there is no way to search a wider area via this
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

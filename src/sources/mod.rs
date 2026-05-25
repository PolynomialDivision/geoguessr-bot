//! Image source abstraction.
//!
//! Each source returns a `GeoImage` — metadata + a download URL (or file path)
//! for the image.  The game layer handles downloading and uploading to Matrix.

pub mod local;
pub mod mapillary;
pub mod wikimedia;

use serde::{Deserialize, Serialize};
use tracing::warn;

// ── Resilient HTTP helper ─────────────────────────────────────────────────────

/// GET `url`, deserialize the JSON body as `T`, retrying on network/parse
/// errors with exponential backoff.  HTTP error status codes are returned
/// as-is (via `error_for_status`) for the caller to handle.
///
/// Delays: 1 s, 2 s, 4 s, 8 s (up to 5 attempts total).
pub(super) async fn get_with_retry<T>(client: &reqwest::Client, url: &str) -> anyhow::Result<T>
where
    T: serde::de::DeserializeOwned,
{
    const MAX_RETRIES: u32 = 5;
    let mut last_err = anyhow::anyhow!("no attempts made");

    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            let delay = 2u64.pow(attempt - 1).min(16);
            warn!("HTTP retry {attempt}/{MAX_RETRIES} for {url} in {delay}s");
            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
        }
        match client.get(url).send().await {
            Err(e) => {
                warn!("HTTP request error: {e}");
                last_err = e.into();
            }
            Ok(resp) => match resp.error_for_status() {
                Err(e) => return Err(e.into()), // HTTP 4xx/5xx — don't retry
                Ok(resp) => match resp.json::<T>().await {
                    Ok(val) => return Ok(val),
                    Err(e)  => {
                        warn!("HTTP response parse error: {e}");
                        last_err = e.into();
                    }
                },
            },
        }
    }

    Err(last_err.context(format!("unreachable after {MAX_RETRIES} attempts: {url}")))
}

/// A located image (or group of images from the same area) ready to be used
/// as a round question.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GeoImage {
    /// The correct country name (matches an entry in `countries::COUNTRIES`).
    pub country:     String,
    /// Continental region (e.g. "Europe", "Asia").
    pub region:      String,
    /// Optional city name shown in the reveal message.
    pub city:        Option<String>,
    /// Direct HTTP(S) URL or absolute file path to the primary image.
    pub image_url:   String,
    /// "wikimedia" | "mapillary" | "local"
    pub source:      String,
    /// Attribution string to include in the reveal (e.g. "© Wikimedia Commons").
    pub attribution: Option<String>,
    /// Actual coordinates of the photo — used for free-guess distance scoring.
    /// Falls back to the country capital if not available.
    pub lat:         Option<f64>,
    pub lon:         Option<f64>,
    /// Additional photos from the same area (optional, for multi-photo questions).
    /// Only the primary `image_url` / `lat` / `lon` are used for scoring.
    #[serde(default)]
    pub extra_image_urls: Vec<String>,
}

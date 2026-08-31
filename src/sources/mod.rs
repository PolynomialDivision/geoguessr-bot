//! Image source abstraction.
//!
//! Each source returns a `GeoImage` — metadata + a download URL (or file path)
//! for the image.  The game layer handles downloading and uploading to Matrix.

pub mod diversity;
pub mod local;
pub mod mapillary;
pub mod quality_filter;

use serde::{Deserialize, Serialize};

// ── Diversity helpers ─────────────────────────────────────────────────────────

/// Minimum distance (km) between any two accepted locations.
pub const MIN_DISTANCE_KM: f64 = 75.0;

/// Haversine great-circle distance in kilometres.
pub(super) fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6371.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    R * 2.0 * a.sqrt().asin()
}

/// Minimum distance from `(lat, lon)` to any point in `existing` (km).
/// Returns `f64::INFINITY` when `existing` is empty (first fetch — always accepted).
pub fn min_dist_to_existing(lat: f64, lon: f64, existing: &[(f64, f64)]) -> f64 {
    existing
        .iter()
        .map(|&(elat, elon)| haversine_km(lat, lon, elat, elon))
        .fold(f64::INFINITY, f64::min)
}

// ── GeoImage ──────────────────────────────────────────────────────────────────

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
    /// Mapillary sequence ID — used to avoid reusing the same capture run.
    #[serde(default)]
    pub sequence:    Option<String>,
    /// Additional photos from the same area (optional, for multi-photo questions).
    /// Only the primary `image_url` / `lat` / `lon` are used for scoring.
    #[serde(default)]
    pub extra_image_urls: Vec<String>,
}

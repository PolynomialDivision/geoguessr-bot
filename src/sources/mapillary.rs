//! Fetches street-level photos from Mapillary (v4 Graph API).
//!
//! Strategy:
//!  1. Pick a random seed country from `countries::COUNTRIES` (optionally
//!     filtered by the configured country allow-list).
//!  2. Call the Mapillary /images endpoint with `closeto=lon,lat&radius=…`.
//!  3. Pick a random image from the results.
//!  4. Return a `GeoImage` with the actual GPS coordinates from the API.
//!
//! Requires a free Mapillary client access token (register at
//! https://www.mapillary.com/developer).

use anyhow::{bail, Result};
use rand::seq::SliceRandom;
use serde::Deserialize;
use tracing::{info, warn};

use super::get_with_retry;

use crate::{
    config::MapillaryConfig,
    countries::{self, Country},
    sources::GeoImage,
};

const API: &str = "https://graph.mapillary.com/images";
const UA:  &str = "geoguessr-bot/0.1 (matrix bot)";

// ── API response shapes ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct MapillaryResp {
    data: Vec<MapillaryImage>,
}

#[derive(Deserialize, Clone)]
struct MapillaryImage {
    #[allow(dead_code)]
    id:              String,
    geometry:        Geometry,
    thumb_1024_url:  Option<String>,
    creator:         Option<Creator>,
}

/// GeoJSON Point geometry — coordinates are [longitude, latitude].
#[derive(Deserialize, Clone)]
struct Geometry {
    coordinates: [f64; 2],
}

#[derive(Deserialize, Clone)]
struct Creator {
    username: Option<String>,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Fetch a `GeoImage` (with up to `n_photos` images) from Mapillary.
///
/// Tries up to 10 different seed countries before giving up.
pub async fn fetch(cfg: &MapillaryConfig, n_photos: usize) -> Result<GeoImage> {
    let n_photos = n_photos.max(1);
    if cfg.access_token.is_empty() {
        bail!("Mapillary: access_token is not configured");
    }

    let client = reqwest::Client::builder()
        .user_agent(UA)
        .build()?;

    let pool: Vec<&Country> = if cfg.countries.is_empty() {
        countries::COUNTRIES.iter().collect()
    } else {
        countries::COUNTRIES
            .iter()
            .filter(|c| cfg.countries.iter().any(|iso| iso.eq_ignore_ascii_case(c.iso)))
            .collect()
    };

    if pool.is_empty() {
        bail!("Mapillary: country filter matches no known countries");
    }

    // Shuffle before any awaits so ThreadRng doesn't cross await points.
    let candidates: Vec<&Country> = {
        let mut rng = rand::thread_rng();
        let mut v = pool.clone();
        v.shuffle(&mut rng);
        v
    };

    for seed in candidates.iter().take(10) {
        match try_seed(&client, seed, cfg, n_photos).await {
            Ok(img) => {
                info!("Mapillary: found {} image(s) for {} ({})", 1 + img.extra_image_urls.len(), seed.name, seed.iso);
                return Ok(img);
            }
            Err(e) => {
                warn!("Mapillary: seed {} failed: {e}", seed.name);
            }
        }
    }

    bail!("Mapillary: could not find a suitable image after trying multiple seeds")
}

// ── Internals ─────────────────────────────────────────────────────────────────

async fn try_seed(
    client:   &reqwest::Client,
    seed:     &Country,
    cfg:      &MapillaryConfig,
    n_photos: usize,
) -> Result<GeoImage> {
    // Mapillary v4 Graph API: proximity search uses separate `lat` and `lng`.
    // The `radius` parameter is in **kilometres** (max 50).
    // Our config stores `search_radius` in metres, so divide by 1000 and clamp.
    let radius_km = (cfg.search_radius / 1000).clamp(1, 50);
    let url = format!(
        "{API}?access_token={token}\
         &fields=id,geometry,thumb_1024_url,creator\
         &lat={lat}&lng={lon}\
         &radius={radius}\
         &limit=50",
        token  = cfg.access_token,
        lat    = seed.lat,
        lon    = seed.lon,
        radius = radius_km,
    );

    let resp: MapillaryResp = get_with_retry(client, &url).await?;

    if resp.data.is_empty() {
        bail!("no images found near {} within {}km", seed.name, radius_km);
    }

    // Shuffle and take up to n_photos (must happen before any further awaits).
    let selected: Vec<MapillaryImage> = {
        let mut rng  = rand::thread_rng();
        let mut data = resp.data;
        data.shuffle(&mut rng);
        // Only keep images that actually have a thumbnail URL.
        data.into_iter()
            .filter(|img| img.thumb_1024_url.as_deref().map(|u| !u.is_empty()).unwrap_or(false))
            .take(n_photos)
            .collect()
    };

    if selected.is_empty() {
        bail!("no images with thumbnail URL near {}", seed.name);
    }

    let primary = &selected[0];

    // GeoJSON: coordinates = [lon, lat]
    let lon = primary.geometry.coordinates[0];
    let lat = primary.geometry.coordinates[1];

    let attribution = primary
        .creator
        .as_ref()
        .and_then(|c| c.username.as_deref())
        .map(|u| format!("© {u} on Mapillary (CC BY-SA)"))
        .unwrap_or_else(|| "© Mapillary contributors (CC BY-SA)".to_owned());

    let extra_image_urls: Vec<String> = selected[1..]
        .iter()
        .filter_map(|img| img.thumb_1024_url.clone())
        .collect();

    Ok(GeoImage {
        country:  seed.name.to_owned(),
        region:   seed.region.to_owned(),
        city:     None,
        image_url: primary.thumb_1024_url.clone().unwrap(), // safe: filtered above
        source:   "mapillary".to_owned(),
        attribution: Some(attribution),
        lat:      Some(lat),
        lon:      Some(lon),
        extra_image_urls,
    })
}

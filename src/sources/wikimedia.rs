//! Fetches geotagged photos from Wikimedia Commons.
//!
//! Strategy:
//!  1. Pick a random seed location from `countries::COUNTRIES` (optionally
//!     filtered by the configured country allow-list).
//!  2. Call the Wikimedia Commons geosearch API to find nearby File: pages.
//!  3. Resolve a thumbnail URL for the first suitable JPEG/PNG result.
//!  4. Return a `GeoImage` whose country is the seed country.
//!
//! No API key required.

use anyhow::{bail, Result};
use rand::seq::SliceRandom;
use serde::Deserialize;
use tracing::{info, warn};

use super::{get_with_retry, min_dist_to_existing, MIN_DISTANCE_KM};

use crate::{
    config::WikimediaConfig,
    countries::{self, Country},
    sources::GeoImage,
};

const API: &str = "https://commons.wikimedia.org/w/api.php";
const UA:  &str = "geoguessr-bot/0.1 (matrix bot; https://github.com/PolynomialDivision)";

// ── API response shapes ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GeoSearchResp {
    query: GeoSearchQuery,
}

#[derive(Deserialize)]
struct GeoSearchQuery {
    geosearch: Vec<GeoResult>,
}

#[derive(Deserialize)]
struct GeoResult {
    pageid: u64,
    #[allow(dead_code)]
    title: String,
    lat:   f64,
    lon:   f64,
}

#[derive(Deserialize)]
struct ImageInfoResp {
    query: ImageInfoQuery,
}

#[derive(Deserialize)]
struct ImageInfoQuery {
    pages: std::collections::HashMap<String, ImagePage>,
}

#[derive(Deserialize)]
struct ImagePage {
    imageinfo: Option<Vec<ImageInfo>>,
}

#[derive(Deserialize)]
struct ImageInfo {
    url:      String,
    thumburl: Option<String>,
    mime:     String,
    size:     u64,
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Fetch a `GeoImage` (with up to `n_photos` images) from Wikimedia Commons.
///
/// `existing` — coordinates of already-cached locations; candidates within
/// MIN_DISTANCE_KM of any existing point are rejected.
///
/// Tries up to 10 different seed locations before giving up.
pub async fn fetch(cfg: &WikimediaConfig, n_photos: usize, existing: &[(f64, f64)]) -> Result<GeoImage> {
    let n_photos = n_photos.max(1);
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
        bail!("Wikimedia source: country filter matches no known countries");
    }

    // Shuffle before any awaits so ThreadRng doesn't cross await points.
    let candidates: Vec<&Country> = {
        let mut rng = rand::thread_rng();
        let mut v = pool.clone();
        v.shuffle(&mut rng);
        v
    };

    // Try up to 10 seed locations.
    for seed in candidates.iter().take(10) {
        match try_seed(&client, seed, cfg, n_photos, existing).await {
            Ok(img) => {
                info!(
                    "Wikimedia: found {} image(s) for {} ({}) — nearest existing {:.0} km",
                    1 + img.extra_image_urls.len(), seed.name, seed.iso,
                    img.lat.zip(img.lon)
                        .map(|(lat, lon)| min_dist_to_existing(lat, lon, existing))
                        .unwrap_or(f64::INFINITY),
                );
                return Ok(img);
            }
            Err(e) => {
                warn!("Wikimedia: seed {} failed: {e}", seed.name);
            }
        }
    }

    bail!("Wikimedia: could not find a suitable image after trying multiple seeds")
}

// ── Internals ─────────────────────────────────────────────────────────────────

async fn try_seed(
    client:   &reqwest::Client,
    seed:     &Country,
    cfg:      &WikimediaConfig,
    n_photos: usize,
    existing: &[(f64, f64)],
) -> Result<GeoImage> {
    // Step 1: geosearch — request more results when we need multiple photos.
    let gslimit = (n_photos * 4).clamp(20, 50); // request extra to survive failures
    let geo_url = format!(
        "{API}?action=query&list=geosearch&gscoord={}|{}&gsradius={}&gslimit={gslimit}\
         &gsnamespace=6&format=json",
        seed.lat, seed.lon, cfg.search_radius
    );
    let resp: GeoSearchResp = get_with_retry(client, &geo_url).await?;

    // Shuffle: the Wikimedia geosearch API returns results sorted by distance
    // from the seed point, so without a shuffle we'd always pick the same
    // nearest photo — shuffling ensures variety across prefetch batches.
    let mut results = resp.query.geosearch;
    {
        let mut rng = rand::thread_rng();
        results.shuffle(&mut rng);
    }

    if results.is_empty() {
        bail!("geosearch returned no results");
    }

    // Step 2: find a primary image whose coordinates pass the distance check.
    let mut primary_result: Option<&GeoResult> = None;

    for result in &results {
        let dist = min_dist_to_existing(result.lat, result.lon, existing);
        if dist < MIN_DISTANCE_KM {
            continue; // too close to an already-cached location
        }
        // Verify we can actually fetch a usable image for this result.
        match get_image_info(client, result.pageid, cfg.max_image_bytes).await {
            Ok(_) => {
                primary_result = Some(result);
                break;
            }
            Err(e) => warn!("Wikimedia: pageId {} skipped: {e}", result.pageid),
        }
    }

    let primary = primary_result.ok_or_else(|| anyhow::anyhow!(
        "no suitable image found among geosearch results (all too close to existing locations \
         or failed image-info fetch)"
    ))?;

    // Step 3: fetch the primary image URL, then collect extra photos from
    // *different* coordinates within the result set (but skip the distance
    // check for extras — they're near the primary by definition).
    let primary_url = get_image_info(client, primary.pageid, cfg.max_image_bytes).await?;

    let mut extra_image_urls: Vec<String> = Vec::with_capacity(n_photos.saturating_sub(1));
    for result in &results {
        if extra_image_urls.len() >= n_photos.saturating_sub(1) { break; }
        if result.pageid == primary.pageid { continue; }
        match get_image_info(client, result.pageid, cfg.max_image_bytes).await {
            Ok(url) => extra_image_urls.push(url),
            Err(e)  => warn!("Wikimedia: extra pageId {} skipped: {e}", result.pageid),
        }
    }

    Ok(GeoImage {
        country:          seed.name.to_owned(),
        region:           seed.region.to_owned(),
        city:             None,
        image_url:        primary_url,
        source:           "wikimedia".to_owned(),
        attribution:      Some("© Wikimedia Commons contributors (CC BY-SA)".to_owned()),
        lat:              Some(primary.lat),
        lon:              Some(primary.lon),
        sequence:         None,
        extra_image_urls,
    })
}

async fn get_image_info(
    client:        &reqwest::Client,
    pageid:        u64,
    max_bytes:     u64,
) -> Result<String> {
    let url = format!(
        "{API}?action=query&pageids={pageid}&prop=imageinfo\
         &iiprop=url|mime|size&iiurlwidth=1280&format=json"
    );
    let resp: ImageInfoResp = get_with_retry(client, &url).await?;

    let page = resp
        .query
        .pages
        .values()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no pages in imageinfo response"))?;

    let info = page
        .imageinfo
        .as_ref()
        .and_then(|v| v.first())
        .ok_or_else(|| anyhow::anyhow!("no imageinfo"))?;

    if !matches!(info.mime.as_str(), "image/jpeg" | "image/png" | "image/webp") {
        bail!("unsupported mime type: {}", info.mime);
    }
    if info.size > max_bytes {
        bail!("image too large: {} bytes", info.size);
    }
    if info.size < 10_000 {
        bail!("image too small: {} bytes", info.size);
    }

    let img_url = info
        .thumburl
        .clone()
        .unwrap_or_else(|| info.url.clone());

    Ok(img_url)
}

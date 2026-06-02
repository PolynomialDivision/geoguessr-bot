//! Fetches street-level photos from Mapillary (v4 Graph API).
//!
//! Strategy:
//!  1. Pick a random seed country from `countries::COUNTRIES` (optionally
//!     filtered by the configured country allow-list).
//!  2. Call the Mapillary /images endpoint with `lat=…&lng=…&radius=…`.
//!  3. Reject any candidate whose coordinates are within MIN_DISTANCE_KM of an
//!     already-cached location, or that share a sequence ID with one.
//!  4. Return a `GeoImage` with the actual GPS coordinates from the API.
//!
//! Requires a free Mapillary client access token (register at
//! https://www.mapillary.com/developer).

use std::{collections::HashMap, sync::Mutex};

use anyhow::{bail, Result};
use rand::seq::SliceRandom;
use serde::Deserialize;
use tracing::{info, warn};

use super::{
    diversity::DiversityTracker,
    get_with_retry, min_dist_to_existing, MIN_DISTANCE_KM,
};

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
    id:              String,
    geometry:        Geometry,
    thumb_1024_url:  Option<String>,
    creator:         Option<Creator>,
    /// Mapillary v4 sequence UUID — images in the same sequence are from the
    /// same capture run on the same road/trail.
    #[serde(default)]
    sequence:        Option<String>,
    /// Capture time as Unix milliseconds (used for freshness scoring).
    #[serde(default)]
    captured_at:     Option<i64>,
    /// Original image dimensions (used for resolution scoring).
    #[serde(default)]
    width:           Option<u32>,
    #[serde(default)]
    height:          Option<u32>,
    /// Camera heading in degrees [0, 360) — used for sequence heading stability.
    #[serde(default)]
    compass_angle:   Option<f32>,
    /// Mapillary server-side quality estimate [0.0, 5.0].
    #[serde(default)]
    quality_score:   Option<f32>,
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
/// `existing` — coordinates of already-cached locations; candidates within
/// MIN_DISTANCE_KM of any existing point are rejected.
///
/// `existing_seqs` — Mapillary sequence IDs already in the cache; candidates
/// sharing a sequence with any cached location are rejected.
///
/// Tries up to 10 different seed countries before giving up.
pub async fn fetch(
    cfg:           &MapillaryConfig,
    n_photos:      usize,
    existing:      &[(f64, f64)],
    existing_seqs: &[Option<String>],
    filter:        &mut super::quality_filter::FilterState,
    blur_cache:    &Mutex<HashMap<String, Option<f32>>>,
) -> Result<GeoImage> {
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

    // Build a diversity tracker from already-accepted locations to detect
    // geographic collapse and prefer under-sampled regions.
    let diversity = DiversityTracker::from_coords(existing);
    if diversity.is_homogeneous() {
        warn!("Mapillary: cache is geographically homogeneous — prioritising under-sampled regions");
    }

    // Shuffle first (so countries with equal diversity scores are tried in
    // random order), then stable-sort by diversity score descending.
    let candidates: Vec<&Country> = {
        let mut rng = rand::thread_rng();
        let mut v = pool.clone();
        v.shuffle(&mut rng);
        v.sort_by(|a, b| {
            diversity.score(b.lat, b.lon)
                .partial_cmp(&diversity.score(a.lat, a.lon))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    };

    for seed in candidates.iter().take(10) {
        match try_seed(&client, seed, cfg, n_photos, existing, existing_seqs, filter, blur_cache).await {
            Ok(Some(img)) => {
                info!(
                    "Mapillary: found {} photo(s) for {} ({}) — nearest existing {:.0} km",
                    1 + img.extra_image_urls.len(),
                    seed.name, seed.iso,
                    img.lat.zip(img.lon)
                        .map(|(lat, lon)| min_dist_to_existing(lat, lon, existing))
                        .unwrap_or(f64::INFINITY),
                );
                return Ok(img);
            }
            Ok(None)   => {} // quality filter rejected all candidates for this seed
            Err(e)     => warn!("Mapillary: seed {} failed: {e}", seed.name),
        }
    }

    bail!("Mapillary: could not find a suitable image after trying multiple seeds")
}

// ── Internals ─────────────────────────────────────────────────────────────────

async fn try_seed(
    client:        &reqwest::Client,
    seed:          &Country,
    cfg:           &MapillaryConfig,
    n_photos:      usize,
    existing:      &[(f64, f64)],
    existing_seqs: &[Option<String>],
    filter:        &mut super::quality_filter::FilterState,
    blur_cache:    &Mutex<HashMap<String, Option<f32>>>,
) -> Result<Option<GeoImage>> {
    let radius_km = (cfg.search_radius / 1000).clamp(1, 50);
    let url = format!(
        "{API}?access_token={token}\
         &fields=id,geometry,thumb_1024_url,creator,sequence,captured_at,width,height,compass_angle,quality_score\
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

    // Save for density scoring before resp.data is moved.
    let area_image_count = resp.data.len();

    // Shuffle, then retain only images that have a thumbnail URL.
    let mut candidates: Vec<MapillaryImage> = {
        let mut rng  = rand::thread_rng();
        let mut data = resp.data;
        data.shuffle(&mut rng);
        data.into_iter()
            .filter(|img| img.thumb_1024_url.as_deref().map(|u| !u.is_empty()).unwrap_or(false))
            .collect()
    };

    if candidates.is_empty() {
        bail!("no images with thumbnail URL near {}", seed.name);
    }

    // ── Collect all distance/sequence-passing candidate indices ─────────────
    let mut passing: Vec<usize> = (0..candidates.len())
        .filter(|&i| {
            let img = &candidates[i];
            let lon = img.geometry.coordinates[0];
            let lat = img.geometry.coordinates[1];
            if min_dist_to_existing(lat, lon, existing) < MIN_DISTANCE_KM {
                return false;
            }
            if let Some(ref seq) = img.sequence {
                if existing_seqs.iter().any(|es| es.as_deref() == Some(seq.as_str())) {
                    return false;
                }
            }
            true
        })
        .collect();

    if passing.is_empty() {
        bail!(
            "all {} candidates near {} are within {MIN_DISTANCE_KM:.0} km of an existing \
             location or share a sequence",
            candidates.len(), seed.name
        );
    }

    // Sort by geographic novelty (under-sampled cells first) so the quality
    // filter sees the most diverse candidates before falling back to familiar
    // regions under anti-starvation pressure.
    let cell_diversity = DiversityTracker::from_coords(existing);
    passing.sort_by(|&a, &b| {
        let sa = cell_diversity.score(
            candidates[a].geometry.coordinates[1],
            candidates[a].geometry.coordinates[0],
        );
        let sb = cell_diversity.score(
            candidates[b].geometry.coordinates[1],
            candidates[b].geometry.coordinates[0],
        );
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });

    // ── Try each diversity-passing candidate through the quality filter ────────
    // Iterating over indices lets us remove and return the first one that passes
    // without cloning the whole struct up front.
    for primary_idx in passing {
        // Clone ID and URL before any borrow of `candidates` crosses an await.
        let img_id    = candidates[primary_idx].id.clone();
        let thumb_url = candidates[primary_idx].thumb_1024_url.clone().unwrap_or_default();

        // Look up cached sharpness score, or download + compute + cache it.
        // The std::sync::Mutex guard is released before every await point.
        let sharpness = {
            let cached = blur_cache.lock()
                .expect("blur_cache lock poisoned")
                .get(&img_id)
                .copied();
            match cached {
                Some(v) => v,
                None => {
                    let result = download_thumbnail(client, &thumb_url).await
                        .and_then(|bytes| compute_sharpness(&bytes));
                    let mut cache = blur_cache.lock()
                        .expect("blur_cache lock poisoned");
                    // Simple size cap — clear when cache grows large.
                    if cache.len() >= 1000 { cache.clear(); }
                    cache.insert(img_id, result);
                    result
                }
            }
        };

        let seq_score = sequence_score_for(&candidates, primary_idx);
        let qr = filter.evaluate(&super::quality_filter::QualityInput {
            width:               candidates[primary_idx].width.unwrap_or(0),
            height:              candidates[primary_idx].height.unwrap_or(0),
            captured_at_ms:      candidates[primary_idx].captured_at,
            area_image_count,
            search_radius_km:    radius_km as f64,
            gps_jitter_m:        None, // not exposed by Mapillary v4 API
            sequence_continuity: seq_score,
            server_quality:      candidates[primary_idx].quality_score,
            sharpness,
        });

        if qr.decision == super::quality_filter::Decision::Reject {
            warn!(
                "Mapillary: quality filter: {} score={:.2} ({})",
                seed.name, qr.score, qr.reason,
            );
            continue; // try next candidate in this seed area
        }

        info!("Mapillary: quality {:.2} ({}) for {}", qr.score, qr.reason, seed.name);

        let primary = candidates.remove(primary_idx);
        let lon = primary.geometry.coordinates[0];
        let lat = primary.geometry.coordinates[1];

        let attribution = primary
            .creator
            .as_ref()
            .and_then(|c| c.username.as_deref())
            .map(|u| format!("© {u} on Mapillary (CC BY-SA)"))
            .unwrap_or_else(|| "© Mapillary contributors (CC BY-SA)".to_owned());

        // Extra photos: from the remaining pool, different sequence from primary.
        let primary_seq = primary.sequence.as_deref();
        let extra_image_urls: Vec<String> = candidates
            .iter()
            .filter(|img| img.sequence.as_deref() != primary_seq || primary_seq.is_none())
            .filter_map(|img| img.thumb_1024_url.clone())
            .take(n_photos.saturating_sub(1))
            .collect();

        return Ok(Some(GeoImage {
            country:         seed.name.to_owned(),
            region:          seed.region.to_owned(),
            city:            None,
            image_url:       primary.thumb_1024_url.unwrap(), // safe: filtered above
            source:          "mapillary".to_owned(),
            attribution:     Some(attribution),
            lat:             Some(lat),
            lon:             Some(lon),
            sequence:        primary.sequence,
            extra_image_urls,
        }));
    }

    // All diversity-passing candidates in this seed area failed quality check.
    Ok(None)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Download a thumbnail image; returns `None` on any network or HTTP error.
async fn download_thumbnail(client: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    if url.is_empty() { return None; }
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() { return None; }
    resp.bytes().await.ok().map(|b| b.to_vec())
}

/// Compute a sharpness score [0.0, 1.0] from image bytes using the Laplacian
/// variance method.  Higher variance means more high-frequency edge content
/// (sharper image).
///
/// Calibration: variance < 200 → 0.0 (blurry), variance ≥ 2000 → 1.0.
/// These thresholds suit 1024 px JPEG thumbnails; motion-blurred dashcam
/// footage typically scores below 0.2.
fn compute_sharpness(img_bytes: &[u8]) -> Option<f32> {
    let img = image::load_from_memory(img_bytes).ok()?.to_luma8();
    let w = img.width() as usize;
    let h = img.height() as usize;
    if w < 3 || h < 3 { return None; }

    let raw = img.as_raw();
    let mut sum_sq = 0.0_f64;
    for y in 1..(h - 1) {
        for x in 1..(w - 1) {
            let c   = raw[y * w + x] as f64;
            let lap = raw[(y - 1) * w + x] as f64
                    + raw[(y + 1) * w + x] as f64
                    + raw[y * w + (x - 1)] as f64
                    + raw[y * w + (x + 1)] as f64
                    - 4.0 * c;
            sum_sq += lap * lap;
        }
    }

    let n = ((w - 2) * (h - 2)) as f64;
    let variance = (sum_sq / n) as f32;

    const BLUR:  f32 = 200.0;
    const SHARP: f32 = 2000.0;
    Some(((variance - BLUR) / (SHARP - BLUR)).clamp(0.0, 1.0))
}

/// Build a sequence-continuity score for the candidate at `primary_idx` by
/// collecting all images from the same sequence ID in `candidates`, sorting
/// them by capture time, and running the sequence scorer.
///
/// Returns `None` when the candidate has no sequence ID (isolated frame
/// without attribution to a traversal), allowing the quality filter to exclude
/// the axis and redistribute its weight rather than blindly penalising.
fn sequence_score_for(
    candidates: &[MapillaryImage],
    primary_idx: usize,
) -> Option<f32> {
    let seq_id = candidates[primary_idx].sequence.as_deref()?;

    let mut frames: Vec<super::quality_filter::SequenceFrame> = candidates
        .iter()
        .filter(|img| img.sequence.as_deref() == Some(seq_id))
        .map(|img| super::quality_filter::SequenceFrame {
            lat:            img.geometry.coordinates[1],
            lon:            img.geometry.coordinates[0],
            captured_at_ms: img.captured_at,
            compass_angle:  img.compass_angle,
        })
        .collect();

    // Sort into traversal order.  Frames without a timestamp keep their
    // original (shuffled) position via a stable sort, which is better than
    // silently placing them at time-0.
    frames.sort_by(|a, b| {
        a.captured_at_ms.unwrap_or(i64::MAX)
            .cmp(&b.captured_at_ms.unwrap_or(i64::MAX))
    });

    Some(super::quality_filter::score_sequence_continuity(&frames))
}

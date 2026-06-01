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

use anyhow::{bail, Result};
use rand::seq::SliceRandom;
use serde::Deserialize;
use tracing::{info, warn};

use super::{get_with_retry, min_dist_to_existing, MIN_DISTANCE_KM};

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

    let candidates: Vec<&Country> = {
        let mut rng = rand::thread_rng();
        let mut v = pool.clone();
        v.shuffle(&mut rng);
        v
    };

    for seed in candidates.iter().take(10) {
        match try_seed(&client, seed, cfg, n_photos, existing, existing_seqs, filter).await {
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
) -> Result<Option<GeoImage>> {
    let radius_km = (cfg.search_radius / 1000).clamp(1, 50);
    let url = format!(
        "{API}?access_token={token}\
         &fields=id,geometry,thumb_1024_url,creator,sequence,captured_at,width,height,compass_angle\
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

    // ── Collect all diversity-passing candidate indices ───────────────────────
    let passing: Vec<usize> = (0..candidates.len())
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

    // ── Try each diversity-passing candidate through the quality filter ────────
    // Iterating over indices lets us remove and return the first one that passes
    // without cloning the whole struct up front.
    for primary_idx in passing {
        let seq_score = sequence_score_for(&candidates, primary_idx);
        let qr = filter.evaluate(&super::quality_filter::QualityInput {
            width:               candidates[primary_idx].width.unwrap_or(0),
            height:              candidates[primary_idx].height.unwrap_or(0),
            captured_at_ms:      candidates[primary_idx].captured_at,
            area_image_count,
            search_radius_km:    radius_km as f64,
            gps_jitter_m:        None, // not exposed by Mapillary v4 API
            sequence_continuity: seq_score,
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

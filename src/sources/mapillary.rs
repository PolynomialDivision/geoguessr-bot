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

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Mutex,
};

use anyhow::{bail, Result};
use rand::seq::SliceRandom;
use serde::Deserialize;
use tracing::{info, warn};

use super::{diversity::DiversityTracker, haversine_km, min_dist_to_existing, MIN_DISTANCE_KM};

use crate::{
    config::MapillaryConfig,
    countries::{self, LocationSeed},
    sources::GeoImage,
};

const API: &str = "https://graph.mapillary.com/images";
const UA: &str = "geoguessr-bot/0.1 (matrix bot)";

/// Max candidate images to request per `bbox` query. Mapillary's documented
/// max (and default) for the `limit` param on /images is 2000; requesting it
/// up front avoids needing pagination to get a representative sample.
const MAPILLARY_IMAGES_LIMIT: u32 = 2000;

/// Half-width, in degrees, of the largest bounding box Mapillary's /images
/// endpoint accepts. The API rejects any bbox that isn't strictly smaller
/// than 0.01 degrees square, so 0.0049 (a 0.0098° full box) stays safely
/// under that with margin for floating-point rounding.
const MAX_BBOX_HALF_DEG: f64 = 0.0049;

/// Number of times a bbox is halved (in both dimensions) after Mapillary
/// rejects it as "too much data" (see `ImagesFetchError::TooMuchData`)
/// before giving up on that seed. This is a *separate* cap from
/// `MAX_BBOX_HALF_DEG`: that one bounds the box the API will accept at all;
/// this one works around an undocumented cap on the total number of matching
/// images per response, which depends on local imagery density and can be
/// tripped well inside the documented size limit in densely-covered cities.
/// 4 halvings take the area down to 1/256th — comfortably below the density
/// that trips the cap even in the world's most densely-mapped city centres —
/// while staying bounded so a pathological case can't spin forever or
/// hammer the API.
const MAX_BBOX_SHRINKS: u32 = 4;

// ── API response shapes ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct MapillaryResp {
    data: Vec<MapillaryImage>,
}

#[derive(Deserialize, Clone)]
struct MapillaryImage {
    id: String,
    geometry: Geometry,
    thumb_1024_url: Option<String>,
    creator: Option<Creator>,
    /// Mapillary v4 sequence UUID — images in the same sequence are from the
    /// same capture run on the same road/trail.
    #[serde(default)]
    sequence: Option<String>,
    /// Capture time as Unix milliseconds (used for freshness scoring).
    #[serde(default)]
    captured_at: Option<i64>,
    /// Original image dimensions (used for resolution scoring).
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    /// Camera heading in degrees [0, 360) — used for sequence heading stability.
    #[serde(default)]
    compass_angle: Option<f32>,
    /// Mapillary server-side quality estimate [0.0, 5.0].
    #[serde(default)]
    quality_score: Option<f32>,
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
/// Per-image metrics: (sharpness_score [0=blurry,1=sharp], overlay_penalty [0=clean,1=severe]).
pub type ImageMetrics = (Option<f32>, Option<f32>);

/// LRU-style in-memory cache of per-image thumbnail metrics.
///
/// Evicts the oldest 20 % of entries when capacity is reached rather than
/// clearing everything, so recently-computed metrics survive across prefetch
/// batches even when the cache is busy.
pub struct BlurCache {
    map: HashMap<String, ImageMetrics>,
    order: VecDeque<String>,
    cap: usize,
}

impl BlurCache {
    pub fn new(cap: usize) -> Self {
        BlurCache {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    pub fn get(&self, key: &str) -> Option<ImageMetrics> {
        self.map.get(key).copied()
    }

    pub fn insert(&mut self, key: String, val: ImageMetrics) {
        if self.map.contains_key(&key) {
            return;
        }
        if self.map.len() >= self.cap {
            let evict = (self.cap / 5).max(1);
            for _ in 0..evict {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, val);
    }
}

pub async fn fetch(
    cfg: &MapillaryConfig,
    n_photos: usize,
    existing: &[(f64, f64)],
    existing_seqs: &[Option<String>],
    filter: &mut super::quality_filter::FilterState,
    blur_cache: &Mutex<BlurCache>,
    skip_countries: &HashSet<String>,
) -> Result<GeoImage> {
    let n_photos = n_photos.max(1);
    if cfg.access_token.is_empty() {
        bail!("Mapillary: access_token is not configured");
    }

    let client = reqwest::Client::builder().user_agent(UA).build()?;

    let all_seeds = countries::location_seeds();
    let full_pool: Vec<&LocationSeed> = if cfg.countries.is_empty() {
        all_seeds.iter().collect()
    } else {
        all_seeds
            .iter()
            .filter(|s| {
                cfg.countries
                    .iter()
                    .any(|iso| iso.eq_ignore_ascii_case(s.country.iso))
            })
            .collect()
    };

    if full_pool.is_empty() {
        bail!("Mapillary: country filter matches no known location seeds");
    }

    // Exclude recently over-represented countries, but keep at least 5 options
    // so the skip list can never starve the pool.
    let filtered: Vec<&LocationSeed> = full_pool
        .iter()
        .copied()
        .filter(|s| {
            !skip_countries.contains(s.country.iso) && !skip_countries.contains(s.country.name)
        })
        .collect();
    let pool = if filtered.len() >= 5 {
        filtered
    } else {
        full_pool
    };

    // Build a diversity tracker from already-accepted locations to detect
    // geographic collapse and prefer under-sampled regions.
    let diversity = DiversityTracker::from_coords(existing);
    if diversity.is_homogeneous() {
        warn!(
            "Mapillary: cache is geographically homogeneous — prioritising under-sampled regions"
        );
    }

    // Shuffle first (so countries with equal diversity scores are tried in
    // random order), then stable-sort by diversity score descending.
    let candidates: Vec<&LocationSeed> = {
        let mut rng = rand::thread_rng();
        let mut v = pool.clone();
        v.shuffle(&mut rng);
        v.sort_by(|a, b| {
            diversity
                .score(b.lat, b.lon)
                .partial_cmp(&diversity.score(a.lat, a.lon))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    };

    // Tallied purely for the diagnostic summary below — every attempt still
    // logs its own WARN as before, so this adds one line on total failure,
    // not per-request spam.
    let mut n_tried = 0u32;
    let mut n_quality_rejected = 0u32;
    let mut n_too_dense = 0u32;
    let mut n_no_images = 0u32;
    let mut n_deduped = 0u32;
    let mut n_other_failures = 0u32;

    for seed in candidates.iter().take(10) {
        n_tried += 1;
        match try_seed(
            &client,
            seed,
            cfg,
            n_photos,
            existing,
            existing_seqs,
            filter,
            blur_cache,
        )
        .await
        {
            Ok(Some(img)) => {
                info!(
                    "Mapillary: found {} photo(s) for {} ({}) — nearest existing {:.0} km",
                    1 + img.extra_image_urls.len(),
                    seed.city,
                    seed.country.iso,
                    img.lat
                        .zip(img.lon)
                        .map(|(lat, lon)| min_dist_to_existing(lat, lon, existing))
                        .unwrap_or(f64::INFINITY),
                );
                return Ok(img);
            }
            Ok(None) => n_quality_rejected += 1, // quality filter rejected all candidates
            Err(e) => {
                match classify_seed_failure(&e.to_string()) {
                    SeedFailureCategory::TooDense => n_too_dense += 1,
                    SeedFailureCategory::NoImages => n_no_images += 1,
                    SeedFailureCategory::Deduped => n_deduped += 1,
                    SeedFailureCategory::Other => n_other_failures += 1,
                }
                warn!(
                    "Mapillary: seed {}, {} failed: {e}",
                    seed.city, seed.country.name
                );
            }
        }
    }

    bail!(
        "Mapillary: could not find a suitable image after trying {n_tried} seed(s) \
         ({n_quality_rejected} quality-rejected, {n_too_dense} too dense for any bbox, \
         {n_no_images} with no images, {n_deduped} fully deduped against history/cache, \
         {n_other_failures} other failures — see preceding WARNs for detail)"
    )
}

// ── Internals ─────────────────────────────────────────────────────────────────

/// Coarse reason bucket for a single seed's failure, used only to build the
/// aggregate diagnostic summary `fetch()` logs when it exhausts all seeds —
/// e.g. "3 too dense, 2 with no images, ..." instead of just "could not find
/// a suitable image", so an admin can tell a density problem from a genuine
/// coverage gap or an over-eager dedup gate without combing through WARNs.
enum SeedFailureCategory {
    /// Every bbox down to the smallest shrink step still had too many
    /// matching images (see `MAX_BBOX_SHRINKS`) — see `try_seed`'s bail!.
    TooDense,
    /// Mapillary has no (or no thumbnail-bearing) images near this seed.
    NoImages,
    /// Every candidate was within `MIN_DISTANCE_KM` of an already-known
    /// location or shared a sequence with one.
    Deduped,
    /// Network error, persistent HTTP error, or anything else.
    Other,
}

/// Classify a `try_seed` failure message (see its `bail!`s) into a
/// `SeedFailureCategory`. Matches on the same literal text `try_seed`
/// bails with, so it only needs updating if those messages change wording.
fn classify_seed_failure(msg: &str) -> SeedFailureCategory {
    if msg.contains("too densely covered") {
        SeedFailureCategory::TooDense
    } else if msg.contains("no images") {
        SeedFailureCategory::NoImages
    } else if msg.contains("of an existing location or share a sequence") {
        SeedFailureCategory::Deduped
    } else {
        SeedFailureCategory::Other
    }
}

async fn try_seed(
    client: &reqwest::Client,
    seed: &LocationSeed,
    cfg: &MapillaryConfig,
    n_photos: usize,
    existing: &[(f64, f64)],
    existing_seqs: &[Option<String>],
    filter: &mut super::quality_filter::FilterState,
    blur_cache: &Mutex<BlurCache>,
) -> Result<Option<GeoImage>> {
    // Mapillary's /images endpoint does NOT support an arbitrarily large
    // point+radius search: its `radius` query param is capped by the API
    // itself at 50 (and per Mapillary's docs that unit is *metres*, not
    // kilometres — confirmed empirically: the API returns HTTP 200 with
    // `{"error": "Param radius must be a number less than or equal to 50"}`
    // for anything above 50). A 50 m circle around an arbitrary seed point
    // yields only a handful of candidate images, which made the
    // photos-per-location quality/diversity filters starve almost
    // everywhere. The endpoint's actual wide-area filter is `bbox`, capped
    // at "smaller than 0.01 degrees square" — empirically this returns
    // ~1000x more candidates than the old radius search for the same seed
    // point, so we use that instead and treat `search_radius` purely as an
    // (optional, smaller-than-max) cap on the box size.
    // Mapillary enforces an undocumented cap on the total number of images
    // matching a bbox (independent of the documented max bbox *size* and of
    // the `limit` query param — confirmed empirically). A box sized at the
    // documented maximum, which is what we start with, routinely exceeds
    // that cap in imagery-dense cities. Rather than lose the whole seed,
    // shrink the box and retry — bounded by MAX_BBOX_SHRINKS so this can
    // never loop forever or burst the API.
    let (mut half_lat_deg, mut half_lon_deg) = seed_half_degs(seed.lat, cfg.search_radius);
    let mut shrinks = 0u32;
    let (resp, left, bottom, right, top) = loop {
        let (left, bottom, right, top) =
            bbox_from_half_degs(seed.lat, seed.lon, half_lat_deg, half_lon_deg);
        let url = format!(
            "{API}?access_token={token}\
             &fields=id,geometry,thumb_1024_url,creator,sequence,captured_at,width,height,compass_angle,quality_score\
             &bbox={left},{bottom},{right},{top}\
             &limit={MAPILLARY_IMAGES_LIMIT}",
            token = cfg.access_token,
        );

        match get_images(client, &url).await {
            Ok(resp) => break (resp, left, bottom, right, top),
            Err(ImagesFetchError::TooMuchData) if shrinks < MAX_BBOX_SHRINKS => {
                shrinks += 1;
                half_lat_deg /= 2.0;
                half_lon_deg /= 2.0;
                warn!(
                    "Mapillary: {} bbox too dense (shrink {shrinks}/{MAX_BBOX_SHRINKS}) — \
                     retrying at {:.4}°×{:.4}°",
                    seed.city,
                    half_lon_deg * 2.0,
                    half_lat_deg * 2.0,
                );
            }
            Err(ImagesFetchError::TooMuchData) => {
                bail!(
                    "too densely covered even at the smallest bbox ({:.5}°×{:.5}°, {} shrink(s))",
                    half_lon_deg * 2.0,
                    half_lat_deg * 2.0,
                    MAX_BBOX_SHRINKS,
                );
            }
            Err(ImagesFetchError::Other(e)) => return Err(e),
        }
    };
    let effective_radius_km = bbox_effective_radius_km(seed.lat, left, bottom, right, top);

    if resp.data.is_empty() {
        bail!(
            "no images found near {}, {} within ~{:.2}km effective radius",
            seed.city,
            seed.country.name,
            effective_radius_km,
        );
    }

    // Save for density scoring before resp.data is moved.
    let area_image_count = resp.data.len();

    // Shuffle, then retain only images that have a thumbnail URL.
    let mut candidates: Vec<MapillaryImage> = {
        let mut rng = rand::thread_rng();
        let mut data = resp.data;
        data.shuffle(&mut rng);
        data.into_iter()
            .filter(|img| {
                img.thumb_1024_url
                    .as_deref()
                    .map(|u| !u.is_empty())
                    .unwrap_or(false)
            })
            .collect()
    };

    if candidates.is_empty() {
        bail!(
            "no images with thumbnail URL near {}, {}",
            seed.city,
            seed.country.name
        );
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
                if existing_seqs
                    .iter()
                    .any(|es| es.as_deref() == Some(seq.as_str()))
                {
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
            candidates.len(),
            seed.city
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
        // Diversity cells are 2° wide (see diversity.rs), so every candidate
        // in a single seed's sub-1km bbox falls in the same cell and ties
        // here in practice. Break ties (and rank within them) by distance to
        // the seed's named point, so we prefer images actually near "Tampere"
        // over ones merely somewhere inside the search box.
        let dist_a = haversine_km(
            seed.lat,
            seed.lon,
            candidates[a].geometry.coordinates[1],
            candidates[a].geometry.coordinates[0],
        );
        let dist_b = haversine_km(
            seed.lat,
            seed.lon,
            candidates[b].geometry.coordinates[1],
            candidates[b].geometry.coordinates[0],
        );
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(dist_a.partial_cmp(&dist_b).unwrap_or(std::cmp::Ordering::Equal))
    });

    // ── Try each diversity-passing candidate through the quality filter ────────
    // Iterating over indices lets us remove and return the first one that passes
    // without cloning the whole struct up front.
    for primary_idx in passing {
        // Clone ID and URL before any borrow of `candidates` crosses an await.
        let img_id = candidates[primary_idx].id.clone();
        let thumb_url = candidates[primary_idx]
            .thumb_1024_url
            .clone()
            .unwrap_or_default();

        // Look up cached metrics, or download thumbnail + compute both in one pass.
        // std::sync::Mutex guards are always released before await points.
        let (sharpness, overlay_penalty) = {
            let cached = blur_cache
                .lock()
                .expect("blur_cache lock poisoned")
                .get(&img_id);
            match cached {
                Some(metrics) => metrics,
                None => {
                    let metrics = match download_thumbnail(client, &thumb_url).await {
                        Some(ref bytes) => (compute_sharpness(bytes), detect_overlay(bytes)),
                        None => (None, None),
                    };
                    blur_cache
                        .lock()
                        .expect("blur_cache lock poisoned")
                        .insert(img_id.clone(), metrics);
                    metrics
                }
            }
        };

        // Sequence-aware overlay: isolated overlay artifacts are penalised less
        // than overlays present across all frames of the same capture run.
        let overlay = overlay_penalty.map(|penalty| {
            let multiplier = sequence_overlay_multiplier(&candidates, primary_idx, blur_cache);
            1.0 - (penalty * multiplier).clamp(0.0, 1.0)
        });

        let seq_score = sequence_score_for(&candidates, primary_idx);
        let qr = filter.evaluate(&super::quality_filter::QualityInput {
            width: candidates[primary_idx].width.unwrap_or(0),
            height: candidates[primary_idx].height.unwrap_or(0),
            captured_at_ms: candidates[primary_idx].captured_at,
            area_image_count,
            search_radius_km: effective_radius_km,
            gps_jitter_m: None, // not exposed by Mapillary v4 API
            sequence_continuity: seq_score,
            server_quality: candidates[primary_idx].quality_score,
            sharpness,
            overlay,
        });

        if qr.decision == super::quality_filter::Decision::Reject {
            warn!(
                "Mapillary: quality filter: {} score={:.2} ({})",
                seed.city, qr.score, qr.reason,
            );
            continue; // try next candidate in this seed area
        }

        info!(
            "Mapillary: quality {:.2} ({}) for {}",
            qr.score, qr.reason, seed.city
        );

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
        // Skip candidates whose cached metrics indicate blur or heavy overlay.
        let primary_seq = primary.sequence.as_deref();
        let extra_image_urls: Vec<String> = candidates
            .iter()
            .filter(|img| img.sequence.as_deref() != primary_seq || primary_seq.is_none())
            .filter(|img| {
                let metrics = blur_cache.lock().ok().and_then(|c| c.get(&img.id));
                match metrics {
                    Some((sharpness, overlay)) => {
                        sharpness.map(|s| s >= 0.2).unwrap_or(true)
                            && overlay.map(|o| o >= 0.3).unwrap_or(true)
                    }
                    None => true,
                }
            })
            .filter_map(|img| img.thumb_1024_url.clone())
            .take(n_photos.saturating_sub(1))
            .collect();

        return Ok(Some(GeoImage {
            country: seed.country.name.to_owned(),
            region: seed.country.region.to_owned(),
            city: Some(seed.city.to_owned()),
            image_url: primary.thumb_1024_url.unwrap(), // safe: filtered above
            source: "mapillary".to_owned(),
            attribution: Some(attribution),
            lat: Some(lat),
            lon: Some(lon),
            sequence: primary.sequence,
            extra_image_urls,
        }));
    }

    // All diversity-passing candidates in this seed area failed quality check.
    Ok(None)
}

/// Outcome of a failed `/images` request, distinguishing the one failure
/// mode `try_seed`'s shrink loop can actually fix from everything else.
enum ImagesFetchError {
    /// HTTP 500 with body `{"error":{"code":1,"message":"Please reduce the
    /// amount of data you're asking for..."}}` — Mapillary's undocumented
    /// cap on the number of images matching a bbox, independent of the
    /// `limit` query param (confirmed empirically: identical failure at
    /// limit=50 and limit=2000 for the same over-dense bbox). Retrying the
    /// identical request never helps; a smaller bbox does.
    TooMuchData,
    /// Anything else (network error, persistent HTTP error, exhausted
    /// transient-error retries, ...) — shrinking the bbox wouldn't help.
    Other(anyhow::Error),
}

/// GET the `/images` endpoint at `url`, retrying network errors, response
/// parse errors, HTTP 429, and other 5xx statuses with the same exponential
/// backoff as the rest of the codebase (1 s, 2 s, 4 s, 8 s; 5 attempts
/// total) — but returning immediately on a persistent 4xx (401/403/404/...,
/// where retrying is pointless) or on the "too much data" 500 (where
/// retrying the *same* request is pointless — see `ImagesFetchError`).
async fn get_images(client: &reqwest::Client, url: &str) -> Result<MapillaryResp, ImagesFetchError> {
    const MAX_RETRIES: u32 = 5;
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            let delay = 2u64.pow(attempt - 1).min(16);
            warn!("Mapillary: HTTP retry {attempt}/{MAX_RETRIES} in {delay}s");
            tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
        }

        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("Mapillary: HTTP request error: {e}");
                last_err = Some(e.into());
                continue;
            }
        };

        if resp.status().is_success() {
            match resp.json::<MapillaryResp>().await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    warn!("Mapillary: HTTP response parse error: {e}");
                    last_err = Some(e.into());
                    continue;
                }
            }
        }

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if status.as_u16() == 500 && body.contains("\"code\":1") {
            return Err(ImagesFetchError::TooMuchData);
        }
        if status.as_u16() == 429 || status.is_server_error() {
            warn!("Mapillary: HTTP {status} (transient) — {body}");
            last_err = Some(anyhow::anyhow!("HTTP {status}: {body}"));
            continue;
        }
        // Persistent client error — retrying the same request won't help.
        return Err(ImagesFetchError::Other(anyhow::anyhow!(
            "HTTP {status}: {body}"
        )));
    }

    Err(ImagesFetchError::Other(
        last_err
            .unwrap_or_else(|| anyhow::anyhow!("no attempts made"))
            .context(format!("unreachable after {MAX_RETRIES} attempts: {url}")),
    ))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a `(left, bottom, right, top)` bounding box around `(lat, lon)`,
/// sized from `radius_m` (metres) but clamped to the largest box Mapillary's
/// /images endpoint will accept (see `MAX_BBOX_HALF_DEG`). Longitude degrees
/// shrink with latitude, so the longitude half-width is widened by
/// `1 / cos(lat)` to keep the box roughly square on the ground — still
/// subject to the same degree clamp, since that's what the API enforces.
///
/// Only `seed_half_degs` + `bbox_from_half_degs` are used at runtime now
/// (`try_seed` needs the halves separately to shrink them); this combined
/// form is kept for the tests below, which predate the shrink loop.
#[cfg(test)]
fn seed_bbox(lat: f64, lon: f64, radius_m: u32) -> (f64, f64, f64, f64) {
    let (half_lat_deg, half_lon_deg) = seed_half_degs(lat, radius_m);
    bbox_from_half_degs(lat, lon, half_lat_deg, half_lon_deg)
}

/// Compute the (half_lat_deg, half_lon_deg) box size for `radius_m` around
/// `lat`, clamped to `MAX_BBOX_HALF_DEG` per side (see its docs). Split out
/// from `seed_bbox` so `try_seed`'s density-driven shrink loop can start
/// from this and shrink both dimensions in place.
fn seed_half_degs(lat: f64, radius_m: u32) -> (f64, f64) {
    const METRES_PER_DEG: f64 = 111_320.0;
    let half_lat_deg = (radius_m as f64 / METRES_PER_DEG).min(MAX_BBOX_HALF_DEG);
    let lon_scale = lat.to_radians().cos().abs().max(0.01); // guard near the poles
    let half_lon_deg = (radius_m as f64 / (METRES_PER_DEG * lon_scale)).min(MAX_BBOX_HALF_DEG);
    (half_lat_deg, half_lon_deg)
}

fn bbox_from_half_degs(lat: f64, lon: f64, half_lat_deg: f64, half_lon_deg: f64) -> (f64, f64, f64, f64) {
    (
        lon - half_lon_deg,
        lat - half_lat_deg,
        lon + half_lon_deg,
        lat + half_lat_deg,
    )
}

/// Radius (km) of a circle with the same area as the given bbox, for feeding
/// into `quality_filter::score_density`, which expects a search radius
/// rather than a box.
fn bbox_effective_radius_km(lat: f64, left: f64, bottom: f64, right: f64, top: f64) -> f64 {
    const KM_PER_DEG: f64 = 111.32;
    let width_km = (right - left) * KM_PER_DEG * lat.to_radians().cos().abs();
    let height_km = (top - bottom) * KM_PER_DEG;
    let area_km2 = (width_km * height_km).max(0.0);
    (area_km2 / std::f64::consts::PI).sqrt()
}

/// Download a thumbnail image; returns `None` on any network or HTTP error.
async fn download_thumbnail(client: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    if url.is_empty() {
        return None;
    }
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.bytes().await.ok().map(|b| b.to_vec())
}

/// Compute a sharpness score [0.0, 1.0] from image bytes using the Laplacian
/// variance method.  Higher variance means more high-frequency edge content
/// (sharper image).
///
/// Calibration: variance < 200 → 0.0 (blurry), variance ≥ 2000 → 1.0.
/// These thresholds suit 1024 px JPEG thumbnails; motion-blurred dashcam
/// footage typically scores below 0.2.
pub(super) fn compute_sharpness(img_bytes: &[u8]) -> Option<f32> {
    let img = image::load_from_memory(img_bytes).ok()?.to_luma8();
    let w = img.width() as usize;
    let h = img.height() as usize;
    if w < 3 || h < 3 {
        return None;
    }

    let raw = img.as_raw();
    let mut sum_sq = 0.0_f64;
    for y in 1..(h - 1) {
        for x in 1..(w - 1) {
            let c = raw[y * w + x] as f64;
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

    const BLUR: f32 = 200.0;
    const SHARP: f32 = 2000.0;
    Some(((variance - BLUR) / (SHARP - BLUR)).clamp(0.0, 1.0))
}

/// Detect UI/banner overlays at the bottom of an image.
///
/// Returns an **overlay penalty** in [0.0, 1.0]: 0.0 = no overlay detected,
/// 1.0 = severe full-width overlay.  The caller converts to a quality score
/// via `1.0 - penalty` before passing to the filter.
///
/// Two complementary signals are combined:
///
/// * **Variance suppression** — a solid-colour UI bar has much lower pixel
///   variance than the rest of the image.  Threshold: strip variance < 35% of
///   full-image variance.
///
/// * **Horizontal edge dominance** — a clear overlay boundary (the top edge of
///   a bar) produces strong horizontal edges.  Measured via Sobel Gy/Gx ratio
///   in the bottom strip; threshold at > 65% of total edge energy being
///   horizontal.
///
/// Both signals use the bottom 20% of the image.
pub(super) fn detect_overlay(img_bytes: &[u8]) -> Option<f32> {
    let img = image::load_from_memory(img_bytes).ok()?.to_luma8();
    let w = img.width() as usize;
    let h = img.height() as usize;
    if w < 10 || h < 10 {
        return None;
    }
    let raw = img.as_raw();

    // Bottom 20% strip (needs ≥ 3 rows for Sobel).
    let strip_y = (h * 4 / 5).max(1);
    if h.saturating_sub(strip_y) < 3 {
        return None;
    }

    // ── Signal 1: variance suppression ───────────────────────────────────────
    let img_sum: f64 = raw.iter().map(|&p| p as f64).sum();
    let img_mean = img_sum / (w * h) as f64;
    let img_var: f64 = raw
        .iter()
        .map(|&p| {
            let d = p as f64 - img_mean;
            d * d
        })
        .sum::<f64>()
        / (w * h) as f64;

    let strip = &raw[strip_y * w..];
    let s_sum: f64 = strip.iter().map(|&p| p as f64).sum();
    let s_mean = s_sum / strip.len() as f64;
    let s_var: f64 = strip
        .iter()
        .map(|&p| {
            let d = p as f64 - s_mean;
            d * d
        })
        .sum::<f64>()
        / strip.len() as f64;

    // 1.0 when strip_var < 35% of full-image variance; 0.0 above that.
    let var_ratio = if img_var > 1.0 {
        (s_var / img_var) as f32
    } else {
        1.0
    };
    let var_signal = (1.0 - var_ratio / 0.35).clamp(0.0, 1.0);

    // ── Signal 2: horizontal edge dominance (Sobel Gy vs Gx) ─────────────────
    let mut gy_sum = 0.0f64; // horizontal-edge response (detects horizontal lines)
    let mut gx_sum = 0.0f64; // vertical-edge response
    for y in (strip_y + 1)..(h.saturating_sub(1)) {
        for x in 1..(w.saturating_sub(1)) {
            macro_rules! px {
                ($r:expr,$c:expr) => {
                    raw[$r * w + $c] as f64
                };
            }
            let gy = -px!(y - 1, x - 1) - 2.0 * px!(y - 1, x) - px!(y - 1, x + 1)
                + px!(y + 1, x - 1)
                + 2.0 * px!(y + 1, x)
                + px!(y + 1, x + 1);
            let gx = -px!(y - 1, x - 1) - 2.0 * px!(y, x - 1) - px!(y + 1, x - 1)
                + px!(y - 1, x + 1)
                + 2.0 * px!(y, x + 1)
                + px!(y + 1, x + 1);
            gy_sum += gy.abs();
            gx_sum += gx.abs();
        }
    }
    let total_edge = gy_sum + gx_sum + 1.0;
    let h_frac = (gy_sum / total_edge) as f32;
    // 0.0 at h_frac = 0.65 (slightly H-dominant but common outdoors);
    // 1.0 at h_frac = 0.90 (overwhelmingly horizontal — clear bar border).
    let hv_signal = ((h_frac - 0.65) / 0.25).clamp(0.0, 1.0);

    // ── Combine ───────────────────────────────────────────────────────────────
    // Variance suppression alone is sufficient for solid bars.
    // Horizontal edge dominance catches banner-border overlays.
    Some((var_signal * 0.55 + hv_signal * 0.45).clamp(0.0, 1.0))
}

/// Compute a sequence-level multiplier for the overlay penalty of `primary_idx`.
///
/// * 0.5  — only one frame visible in this candidate pool → possibly isolated.
/// * 0.6  — fewer than 25% of cached peers have high overlay → isolated artifact.
/// * 1.0  — no peer cache data, or overlay is widespread across the sequence.
fn sequence_overlay_multiplier(
    candidates: &[MapillaryImage],
    primary_idx: usize,
    blur_cache: &Mutex<BlurCache>,
) -> f32 {
    let seq_id = match candidates[primary_idx].sequence.as_deref() {
        Some(s) => s,
        None => return 1.0,
    };

    let peer_ids: Vec<&str> = candidates
        .iter()
        .enumerate()
        .filter(|&(i, img)| i != primary_idx && img.sequence.as_deref() == Some(seq_id))
        .map(|(_, img)| img.id.as_str())
        .collect();

    if peer_ids.is_empty() {
        return 0.5; // no other frames from this sequence in the pool
    }

    let cache = blur_cache.lock().expect("blur_cache lock");
    let peer_overlays: Vec<f32> = peer_ids
        .iter()
        .filter_map(|id| cache.get(id))
        .filter_map(|(_, penalty)| penalty)
        .collect();
    drop(cache);

    if peer_overlays.is_empty() {
        return 1.0; // peers exist but uncached — apply full penalty conservatively
    }

    let high_frac =
        peer_overlays.iter().filter(|&&s| s > 0.5).count() as f32 / peer_overlays.len() as f32;

    if high_frac < 0.25 {
        0.6
    } else {
        1.0
    }
}

/// Build a sequence-continuity score for the candidate at `primary_idx` by
/// collecting all images from the same sequence ID in `candidates`, sorting
/// them by capture time, and running the sequence scorer.
///
/// Returns `None` when the candidate has no sequence ID (isolated frame
/// without attribution to a traversal), allowing the quality filter to exclude
/// the axis and redistribute its weight rather than blindly penalising.
fn sequence_score_for(candidates: &[MapillaryImage], primary_idx: usize) -> Option<f32> {
    let seq_id = candidates[primary_idx].sequence.as_deref()?;

    let mut frames: Vec<super::quality_filter::SequenceFrame> = candidates
        .iter()
        .filter(|img| img.sequence.as_deref() == Some(seq_id))
        .map(|img| super::quality_filter::SequenceFrame {
            lat: img.geometry.coordinates[1],
            lon: img.geometry.coordinates[0],
            captured_at_ms: img.captured_at,
            compass_angle: img.compass_angle,
        })
        .collect();

    // Sort into traversal order.  Frames without a timestamp keep their
    // original (shuffled) position via a stable sort, which is better than
    // silently placing them at time-0.
    frames.sort_by(|a, b| {
        a.captured_at_ms
            .unwrap_or(i64::MAX)
            .cmp(&b.captured_at_ms.unwrap_or(i64::MAX))
    });

    Some(super::quality_filter::score_sequence_continuity(&frames))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_bbox_never_exceeds_mapillarys_bbox_size_limit() {
        // Mapillary rejects any bbox that isn't strictly smaller than 0.01
        // degrees square. Check both a large requested radius and a
        // near-polar latitude (where 1/cos(lat) blows up the naive
        // longitude conversion) stay safely under that in both dimensions.
        for (lat, lon, radius_m) in [
            (0.0, 0.0, 50_000),
            (61.4978, 23.7610, 50_000), // Tampere
            (65.0, 20.0, 200_000),      // near-polar, huge requested radius
            (-33.0, 151.0, 5_000),      // small requested radius
        ] {
            let (left, bottom, right, top) = seed_bbox(lat, lon, radius_m);
            assert!(right - left < 0.01, "lon span too large at lat={lat}");
            assert!(top - bottom < 0.01, "lat span too large at lat={lat}");
            assert!(left < lon && lon < right);
            assert!(bottom < lat && lat < top);
        }
    }

    #[test]
    fn seed_bbox_respects_small_requested_radius() {
        // A radius well under the API ceiling should not be clamped up to it.
        let (_left, bottom, _right, top) = seed_bbox(0.0, 0.0, 100);
        let lat_span_deg = top - bottom;
        assert!(lat_span_deg < MAX_BBOX_HALF_DEG); // much smaller than the max
        assert!(lat_span_deg > 0.0);
    }

    #[test]
    fn bbox_effective_radius_km_is_positive_and_reasonable() {
        let (left, bottom, right, top) = seed_bbox(48.85, 2.35, 50_000); // Paris
        let r = bbox_effective_radius_km(48.85, left, bottom, right, top);
        // Max bbox is roughly ~0.5-0.6 km on a side at this latitude, so the
        // equivalent-area radius should land well under 1 km.
        assert!(r > 0.0 && r < 1.0, "effective radius out of range: {r}");
    }

    // ── Bbox-density shrink (root-cause regression) ─────────────────────────

    #[test]
    fn bbox_shrink_halves_both_dimensions_and_stays_positive_through_max_shrinks() {
        // Regression for the "too much data" bug: a bbox sized at the
        // documented max routinely exceeded Mapillary's undocumented
        // per-response density cap in imagery-dense cities (see try_seed).
        // Each shrink step must genuinely shrink the box and never reach
        // zero/negative, across the full bound.
        let (start_lat, start_lon) = seed_half_degs(45.07, 50_000); // dense-city-like start
        let (mut half_lat, mut half_lon) = (start_lat, start_lon);
        for step in 1..=MAX_BBOX_SHRINKS {
            let (prev_lat, prev_lon) = (half_lat, half_lon);
            half_lat /= 2.0;
            half_lon /= 2.0;
            assert!(
                half_lat > 0.0 && half_lon > 0.0,
                "shrink {step} produced a non-positive half-degree"
            );
            assert!(half_lat < prev_lat && half_lon < prev_lon, "shrink {step} did not shrink");
        }
        // 4 halvings should take the area down to 1/256th.
        assert!((half_lat - start_lat / 16.0).abs() < 1e-12);
        assert!((half_lon - start_lon / 16.0).abs() < 1e-12);
    }

    #[test]
    fn classify_seed_failure_buckets_each_try_seed_bail_message() {
        assert!(matches!(
            classify_seed_failure(
                "too densely covered even at the smallest bbox (0.00031°×0.00031°, 4 shrink(s))"
            ),
            SeedFailureCategory::TooDense
        ));
        assert!(matches!(
            classify_seed_failure("no images found near Mombasa, Kenya within ~0.56km effective radius"),
            SeedFailureCategory::NoImages
        ));
        assert!(matches!(
            classify_seed_failure(
                "all 360 candidates near Iran are within 75 km of an existing location or share a sequence"
            ),
            SeedFailureCategory::Deduped
        ));
        assert!(matches!(
            classify_seed_failure("HTTP 403: forbidden"),
            SeedFailureCategory::Other
        ));
    }

    // ── get_images: transient vs. persistent vs. too-much-data (root cause) ─

    /// Spawns a local HTTP server that serves canned `(status, body)`
    /// responses for successive requests to `/images` (repeating the last
    /// entry once exhausted), for exercising `get_images`'s retry/
    /// classification logic against real HTTP semantics without touching
    /// the real Mapillary API. Returns the request URL, a request counter,
    /// and the server's task handle (keep it alive for the test's duration).
    async fn spawn_canned_server(
        responses: Vec<(u16, &'static str)>,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let responses = Arc::new(responses);
        let counter = Arc::new(AtomicUsize::new(0));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = {
            let responses = responses.clone();
            let counter = counter.clone();
            axum::Router::new().route(
                "/images",
                axum::routing::get(move || {
                    let responses = responses.clone();
                    let counter = counter.clone();
                    async move {
                        let i = counter.fetch_add(1, Ordering::SeqCst);
                        let (status, body) = responses[i.min(responses.len() - 1)];
                        (axum::http::StatusCode::from_u16(status).unwrap(), body.to_owned())
                    }
                }),
            )
        };

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        (format!("http://{addr}/images"), counter, handle)
    }

    #[tokio::test]
    async fn get_images_classifies_the_real_mapillary_too_much_data_response() {
        // Exact body captured from the live API for a bbox in a dense city
        // (this is the actual root cause of the "no location with enough
        // valid images" abort). Must be recognised without retrying — the
        // identical request would just fail identically again.
        let (url, counter, _server) = spawn_canned_server(vec![(
            500,
            r#"{"error":{"code":1,"message":"Please reduce the amount of data you're asking for, then retry your request"}}"#,
        )])
        .await;
        let client = reqwest::Client::new();

        let result = get_images(&client, &url).await;

        assert!(matches!(result, Err(ImagesFetchError::TooMuchData)));
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "must not retry the identical too-much-data request"
        );
    }

    #[tokio::test]
    async fn get_images_retries_a_transient_error_then_succeeds() {
        let (url, counter, _server) = spawn_canned_server(vec![
            (503, "temporarily unavailable"),
            (200, r#"{"data":[]}"#),
        ])
        .await;
        let client = reqwest::Client::new();

        let result = get_images(&client, &url).await;

        assert!(result.is_ok(), "expected recovery after one transient error");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn get_images_does_not_retry_a_persistent_client_error() {
        // 403/404/etc. won't be fixed by retrying — retrying anyway would be
        // exactly the API-hammering behaviour this fix must avoid.
        let (url, counter, _server) =
            spawn_canned_server(vec![(403, r#"{"error":{"message":"forbidden"}}"#)]).await;
        let client = reqwest::Client::new();

        let result = get_images(&client, &url).await;

        assert!(matches!(result, Err(ImagesFetchError::Other(_))));
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "must not retry a persistent 4xx"
        );
    }

    #[tokio::test]
    async fn get_images_gives_up_cleanly_after_bounded_retries_on_full_outage() {
        // A source that is down for every request must still terminate with
        // a clean error in bounded time and a bounded number of requests —
        // never hang or retry forever.
        let (url, counter, _server) =
            spawn_canned_server(vec![(500, "internal server error")]).await; // not the too-much-data body
        let client = reqwest::Client::new();

        let result = tokio::time::timeout(std::time::Duration::from_secs(30), get_images(&client, &url))
            .await
            .expect("get_images must terminate on its own, not hang forever");

        assert!(matches!(result, Err(ImagesFetchError::Other(_))));
        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            5,
            "must attempt exactly MAX_RETRIES times, not loop forever"
        );
    }
}

//! Local directory image source.
//!
//! Reads images from a directory containing an `index.json` file:
//!
//! ```json
//! [
//!   {
//!     "file": "berlin.jpg",
//!     "country": "Germany",
//!     "region": "Europe",
//!     "city": "Berlin",
//!     "attribution": "Photo by Alice"
//!   },
//!   ...
//! ]
//! ```
//!
//! `file` is relative to the directory containing `index.json`.
//! `country` must match an entry in `countries::COUNTRIES`.

use anyhow::{bail, Context, Result};
use rand::seq::SliceRandom;
use serde::Deserialize;

use crate::sources::GeoImage;

#[derive(Deserialize)]
struct IndexEntry {
    file:         String,
    country:      String,
    region:       String,
    city:         Option<String>,
    attribution:  Option<String>,
    lat:          Option<f64>,
    lon:          Option<f64>,
}

pub async fn fetch(dir: &str) -> Result<GeoImage> {
    let index_path = std::path::Path::new(dir).join("index.json");
    let raw = tokio::fs::read_to_string(&index_path)
        .await
        .with_context(|| format!("Reading {}", index_path.display()))?;

    let entries: Vec<IndexEntry> = serde_json::from_str(&raw)
        .context("Parsing index.json")?;

    if entries.is_empty() {
        bail!("local source: index.json is empty");
    }

    let mut rng  = rand::thread_rng();
    let mut pool: Vec<&IndexEntry> = entries.iter().collect();
    pool.shuffle(&mut rng);

    let entry = pool
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("local source: no entries"))?;

    let abs_path = std::path::Path::new(dir)
        .join(&entry.file)
        .to_string_lossy()
        .to_string();

    Ok(GeoImage {
        country:          entry.country.clone(),
        region:           entry.region.clone(),
        city:             entry.city.clone(),
        image_url:        abs_path,
        source:           "local".to_owned(),
        attribution:      entry.attribution.clone(),
        lat:              entry.lat,
        lon:              entry.lon,
        sequence:         None,
        extra_image_urls: vec![],
    })
}

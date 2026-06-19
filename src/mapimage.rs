//! Render static map PNGs for GeoGuessr results.
//!
//! Uses OpenStreetMap tiles via the `staticmap` crate.
//! All tile fetching is synchronous — call via `tokio::task::spawn_blocking`.

use staticmap::{
    lat_to_y,
    tools::{CircleBuilder, Color, IconBuilder, LineBuilder},
    StaticMapBuilder,
};
use crate::avatar::{PIN_ANCHOR_X, PIN_ANCHOR_Y};

const GUESS_MAP_W: u32 = 640;
const GUESS_MAP_H: u32 = 400;
const GUESS_MAP_MAX_ZOOM: u8 = 15;
const GUESS_MAP_EFFECTIVE_W: f64 = 520.0;
const GUESS_MAP_EFFECTIVE_H: f64 = 260.0;

// ── Colour palette for multi-player round maps ────────────────────────────────
// (R, G, B, chat_emoji)  — visually distinct, works on both light and dark maps.
// `pub` so game.rs can look up the colour for each player when rendering pins.
pub const PLAYER_COLORS: &[(u8, u8, u8, &str)] = &[
    ( 50, 120, 255, "🔵"),  // blue
    (220,  60,  60, "🔴"),  // red
    ( 50, 195,  80, "🟢"),  // green
    (180,  50, 220, "🟣"),  // purple
    (210, 165,   0, "🟡"),  // yellow
    (  0, 185, 210, "🩵"),  // cyan
    (240, 120,   0, "🟠"),  // orange
    (200,   0, 140, "🩷"),  // pink
];

// ── Single-player map ─────────────────────────────────────────────────────────

/// Render a 640×400 PNG showing one guess vs the actual location.
///
/// * `player_pin` – pre-rendered avatar pin PNG (from `avatar::render_avatar_pin`).
///   If `None`, a plain filled circle is drawn instead.
/// * `(r, g, b)` – the player's colour (used for the line and the circle fallback).
///
/// Zoom is chosen automatically based on `dist_km`.
/// Returns `None` if tile fetching or encoding fails.
pub fn render_guess_map(
    guess_lat:  f64,
    guess_lon:  f64,
    actual_lat: f64,
    actual_lon: f64,
    dist_km:    f64,
    player_pin: Option<Vec<u8>>,
    r: u8, g: u8, b: u8,
) -> Option<Vec<u8>> {
    let center_lat = (guess_lat + actual_lat) / 2.0;
    let (center_lon, arc_span) = minimum_lon_arc(&[guess_lon, actual_lon]);
    let guess_lon  = normalize_lon(guess_lon,  center_lon);
    let actual_lon = normalize_lon(actual_lon, center_lon);

    let zoom = guess_map_zoom(guess_lat, actual_lat, arc_span, dist_km);

    let mut map = StaticMapBuilder::new()
        .width(GUESS_MAP_W)
        .height(GUESS_MAP_H)
        .zoom(zoom)
        .lat_center(center_lat)
        .lon_center(center_lon)
        .url_template("https://a.basemaps.cartocdn.com/rastertiles/voyager/{z}/{x}/{y}.png")
        .build()
        .ok()?;

    add_line_shorter_arc(&mut map, guess_lat, guess_lon, actual_lat, actual_lon, r, g, b);

    // Avatar pin if available; plain circle as fallback.
    let placed = player_pin.and_then(|png| {
        IconBuilder::new()
            .lat_coordinate(guess_lat)
            .lon_coordinate(guess_lon)
            .x_offset(PIN_ANCHOR_X)
            .y_offset(PIN_ANCHOR_Y)
            .data(png.as_slice())
            .and_then(|b| b.build())
            .ok()
    });
    if let Some(icon) = placed {
        map.add_tool(icon);
    } else {
        add_circle(&mut map, guess_lat, guess_lon, r, g, b);
    }

    add_actual_marker(&mut map, actual_lat, actual_lon);

    map.encode_png().ok()
}

// ── Round summary map (all guesses) ──────────────────────────────────────────

/// Render a 700×500 PNG showing every player's guess and the actual location.
///
/// `guesses` is `(display_name, lat, lon, avatar_pin_png)`.
/// `avatar_pin_png` is a pre-rendered 40×52 PNG produced by
/// `avatar::render_avatar_pin`.  Pass `None` to fall back to a plain circle.
///
/// Returns `(png_bytes, legend)` where `legend` is a `Vec<(display_name, emoji)>`
/// in the same order as `guesses`, ready for posting as a chat message.
///
/// The map uses the minimum-width arc to fit all points, with antimeridian-safe
/// line drawing so lines always show the shorter path.
pub fn render_round_map(
    guesses:    &[(String, f64, f64, Option<Vec<u8>>)],   // (name, lat, lon, pin_png)
    actual_lat: f64,
    actual_lon: f64,
) -> Option<(Vec<u8>, Vec<(String, &'static str)>)> {
    // ── Compute explicit center + zoom from minimum-width arc ─────────────────
    let all_lons: Vec<f64> = std::iter::once(actual_lon)
        .chain(guesses.iter().map(|(_, _, lon, _)| *lon))
        .collect();
    let all_lats: Vec<f64> = std::iter::once(actual_lat)
        .chain(guesses.iter().map(|(_, lat, _, _)| *lat))
        .collect();

    let (center_lon, lon_span) = minimum_lon_arc(&all_lons);
    let actual_lon = normalize_lon(actual_lon, center_lon);
    let lat_min = all_lats.iter().cloned().fold(f64::INFINITY,     f64::min);
    let lat_max = all_lats.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let center_lat = (lat_min + lat_max) / 2.0;

    // Max zoom where lon span fits in effective width (700 - 2×40 = 620 px).
    let max_zoom_lon = if lon_span > 0.0 {
        ((620.0_f64 / 256.0 * 360.0) / lon_span).log2().floor().clamp(0.0, 11.0) as u8
    } else { 11u8 };

    // Max zoom where lat span fits in effective height (500 - 2×40 = 420 px).
    let max_zoom_lat = (0u8..=11).rev()
        .find(|&z| {
            let y_span = (lat_to_y(lat_min, z) - lat_to_y(lat_max, z)).abs() * 256.0;
            y_span <= 420.0
        })
        .unwrap_or(0);

    let zoom = max_zoom_lon.min(max_zoom_lat);

    let mut map = StaticMapBuilder::new()
        .width(700)
        .height(500)
        .zoom(zoom)
        .lat_center(center_lat)
        .lon_center(center_lon)
        .url_template("https://a.basemaps.cartocdn.com/rastertiles/voyager/{z}/{x}/{y}.png")
        .build()
        .ok()?;

    // ── Lines first so they render behind the markers ─────────────────────────
    for (idx, (_name, lat, lon, _pin)) in guesses.iter().enumerate() {
        let lon_n = normalize_lon(*lon, center_lon);
        let (r, g, b, _) = PLAYER_COLORS[idx % PLAYER_COLORS.len()];
        add_line_shorter_arc(&mut map, *lat, lon_n, actual_lat, actual_lon, r, g, b);
    }

    // ── Player markers (avatar pin or plain circle fallback) ──────────────────
    let mut legend: Vec<(String, &'static str)> = Vec::new();
    for (idx, (name, lat, lon, pin_png)) in guesses.iter().enumerate() {
        let lon_n = normalize_lon(*lon, center_lon);
        let (r, g, b, emoji) = PLAYER_COLORS[idx % PLAYER_COLORS.len()];

        let placed = pin_png.as_ref().and_then(|png| {
            IconBuilder::new()
                .lat_coordinate(*lat)
                .lon_coordinate(lon_n)
                .x_offset(PIN_ANCHOR_X)
                .y_offset(PIN_ANCHOR_Y)
                .data(png.as_slice())
                .and_then(|b| b.build())
                .ok()
        });

        if let Some(icon) = placed {
            map.add_tool(icon);
        } else {
            add_circle(&mut map, *lat, lon_n, r, g, b);
        }

        legend.push((name.clone(), emoji));
    }

    // ── Actual location: large white ring + dark fill (stands out from players)
    if let Ok(ring) = CircleBuilder::new()
        .lat_coordinate(actual_lat)
        .lon_coordinate(actual_lon)
        .color(Color::new(true, 255, 255, 255, 255))
        .radius(15.0)
        .build()
    {
        map.add_tool(ring);
    }
    if let Ok(fill) = CircleBuilder::new()
        .lat_coordinate(actual_lat)
        .lon_coordinate(actual_lon)
        .color(Color::new(true, 20, 20, 20, 255))
        .radius(9.0)
        .build()
    {
        map.add_tool(fill);
    }

    let png = map.encode_png().ok()?;
    Some((png, legend))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Normalize `lon` to be within 180° of `center_lon`.
/// May return values outside [-180, 180]; staticmap's tile arithmetic handles
/// that correctly for pixel placement even though tile fetching wraps separately.
fn normalize_lon(lon: f64, center_lon: f64) -> f64 {
    let mut l = lon;
    while l - center_lon >  180.0 { l -= 360.0; }
    while l - center_lon < -180.0 { l += 360.0; }
    l
}

/// Returns `(center_lon, arc_span_degrees)` for the minimum-width arc
/// containing all given longitudes.  Handles antimeridian wrapping correctly.
fn minimum_lon_arc(lons: &[f64]) -> (f64, f64) {
    if lons.is_empty() { return (0.0, 360.0); }

    // Convert lons to [0, 360) fractions, sort, deduplicate.
    let mut fracs: Vec<f64> = lons.iter()
        .map(|&l| (l + 180.0).rem_euclid(360.0))
        .collect();
    fracs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    fracs.dedup_by(|a, b| (*a - *b).abs() < 1e-9);

    let n = fracs.len();
    if n == 1 { return (fracs[0] - 180.0, 0.0); }

    // Find the largest gap between consecutive fracs (including the wrap-around gap).
    let mut max_gap    = 0.0_f64;
    let mut arc_start  = fracs[0];
    for i in 0..n {
        let gap = if i + 1 < n {
            fracs[i + 1] - fracs[i]
        } else {
            360.0 - fracs[n - 1] + fracs[0]  // wrap-around gap
        };
        if gap > max_gap {
            max_gap   = gap;
            arc_start = if i + 1 < n { fracs[i + 1] } else { fracs[0] };
        }
    }

    let span        = 360.0 - max_gap;
    let center_frac = (arc_start + span / 2.0).rem_euclid(360.0);
    let center_lon  = center_frac - 180.0;
    (center_lon, span)
}

fn guess_map_zoom(guess_lat: f64, actual_lat: f64, lon_span: f64, dist_km: f64) -> u8 {
    let max_zoom_lon = if lon_span > 0.0 {
        ((GUESS_MAP_EFFECTIVE_W / 256.0 * 360.0) / lon_span)
            .log2()
            .floor()
            .clamp(0.0, GUESS_MAP_MAX_ZOOM as f64) as u8
    } else {
        GUESS_MAP_MAX_ZOOM
    };

    let lat_min = guess_lat.min(actual_lat);
    let lat_max = guess_lat.max(actual_lat);
    let max_zoom_lat = (0u8..=GUESS_MAP_MAX_ZOOM).rev()
        .find(|&z| {
            let y_span = (lat_to_y(lat_min, z) - lat_to_y(lat_max, z)).abs() * 256.0;
            y_span <= GUESS_MAP_EFFECTIVE_H
        })
        .unwrap_or(0);

    let fit_zoom = max_zoom_lon.min(max_zoom_lat);
    let distance_cap = match dist_km {
        d if d <= 0.5 => 15,
        d if d <= 2.0 => 14,
        d if d <= 8.0 => 13,
        d if d <= 20.0 => 12,
        d if d <= 80.0 => 10,
        d if d <= 250.0 => 8,
        d if d <= 700.0 => 7,
        d if d <= 2_000.0 => 6,
        d if d <= 5_000.0 => 5,
        _ => 4,
    };

    fit_zoom.min(distance_cap)
}

/// Draw a line taking the SHORTER arc between two points.
/// When the shorter arc crosses the antimeridian the line is split into two
/// segments at ±180° so the staticmap crate renders it correctly.
fn add_line_shorter_arc(
    map: &mut staticmap::StaticMap,
    lat1: f64, lon1: f64,
    lat2: f64, lon2: f64,
    r: u8, g: u8, b: u8,
) {
    let diff = lon2 - lon1;
    if diff.abs() <= 180.0 {
        add_line(map, lat1, lon1, lat2, lon2, r, g, b);
        return;
    }
    // Shorter arc crosses the antimeridian.  Express lon2 in the direction
    // that crosses ±180° so we can interpolate the crossing latitude.
    let (lon2_ext, cross_lon) = if diff > 0.0 {
        (lon2 - 360.0, -180.0_f64)  // shorter path goes west
    } else {
        (lon2 + 360.0,  180.0_f64)  // shorter path goes east
    };
    let t         = (cross_lon - lon1) / (lon2_ext - lon1);
    let cross_lat = lat1 + t * (lat2 - lat1);
    add_line(map, lat1,      lon1,       cross_lat, cross_lon,  r, g, b);
    add_line(map, cross_lat, -cross_lon, lat2,      lon2,       r, g, b);
}

/// Draw a white-underlined coloured line between two points.
fn add_line(
    map: &mut staticmap::StaticMap,
    lat1: f64, lon1: f64,
    lat2: f64, lon2: f64,
    r: u8, g: u8, b: u8,
) {
    if let Ok(ul) = LineBuilder::new()
        .lat_coordinates(vec![lat1, lat2])
        .lon_coordinates(vec![lon1, lon2])
        .color(Color::new(true, 255, 255, 255, 160))
        .width(5.0)
        .simplify(true)
        .build()
    {
        map.add_tool(ul);
    }
    if let Ok(ln) = LineBuilder::new()
        .lat_coordinates(vec![lat1, lat2])
        .lon_coordinates(vec![lon1, lon2])
        .color(Color::new(true, r, g, b, 210))
        .width(2.5)
        .simplify(true)
        .build()
    {
        map.add_tool(ln);
    }
}

/// Draw a white-bordered filled circle at a given location.
fn add_circle(
    map: &mut staticmap::StaticMap,
    lat: f64, lon: f64,
    r: u8, g: u8, b: u8,
) {
    if let Ok(border) = CircleBuilder::new()
        .lat_coordinate(lat)
        .lon_coordinate(lon)
        .color(Color::new(true, 255, 255, 255, 255))
        .radius(11.0)
        .build()
    {
        map.add_tool(border);
    }
    if let Ok(fill) = CircleBuilder::new()
        .lat_coordinate(lat)
        .lon_coordinate(lon)
        .color(Color::new(true, r, g, b, 255))
        .radius(8.0)
        .build()
    {
        map.add_tool(fill);
    }
}

/// Draw the "actual location" marker: large white ring + dark fill.
/// Uses the same style as `render_round_map` so both maps are consistent.
fn add_actual_marker(map: &mut staticmap::StaticMap, lat: f64, lon: f64) {
    if let Ok(ring) = CircleBuilder::new()
        .lat_coordinate(lat)
        .lon_coordinate(lon)
        .color(Color::new(true, 255, 255, 255, 255))
        .radius(15.0)
        .build()
    {
        map.add_tool(ring);
    }
    if let Ok(fill) = CircleBuilder::new()
        .lat_coordinate(lat)
        .lon_coordinate(lon)
        .color(Color::new(true, 20, 20, 20, 255))
        .radius(9.0)
        .build()
    {
        map.add_tool(fill);
    }
}

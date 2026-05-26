//! Render static map PNGs for GeoGuessr results.
//!
//! Uses OpenStreetMap tiles via the `staticmap` crate.
//! All tile fetching is synchronous — call via `tokio::task::spawn_blocking`.

use staticmap::{
    tools::{CircleBuilder, Color, LineBuilder},
    StaticMapBuilder,
};

// ── Colour palette for multi-player round maps ────────────────────────────────
// (R, G, B, chat_emoji)  — visually distinct, works on both light and dark maps.
const PLAYER_COLORS: &[(u8, u8, u8, &str)] = &[
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

/// Render a 640×400 PNG showing one guess (🔵) vs the actual location (🔴).
/// Zoom is chosen automatically based on `dist_km`.
/// Returns `None` if tile fetching or encoding fails.
pub fn render_guess_map(
    guess_lat:  f64,
    guess_lon:  f64,
    actual_lat: f64,
    actual_lon: f64,
    dist_km:    f64,
) -> Option<Vec<u8>> {
    let center_lat = (guess_lat + actual_lat) / 2.0;
    let center_lon = (guess_lon + actual_lon) / 2.0;

    let zoom: u8 = match dist_km as u32 {
        0..=20      => 11,
        21..=80     => 9,
        81..=250    => 7,
        251..=700   => 6,
        701..=2000  => 5,
        2001..=5000 => 4,
        _           => 3,
    };

    let mut map = StaticMapBuilder::new()
        .width(640)
        .height(400)
        .zoom(zoom)
        .lat_center(center_lat)
        .lon_center(center_lon)
        .url_template("https://a.tile.osm.org/{z}/{x}/{y}.png")
        .build()
        .ok()?;

    add_line(&mut map, guess_lat, guess_lon, actual_lat, actual_lon, 255, 140, 0);
    add_circle(&mut map, guess_lat, guess_lon, 50, 100, 255);
    add_actual_marker(&mut map, actual_lat, actual_lon);

    map.encode_png().ok()
}

// ── Round summary map (all guesses) ──────────────────────────────────────────

/// Render a 700×500 PNG showing every player's guess and the actual location.
///
/// Returns `(png_bytes, legend)` where `legend` is a `Vec<(display_name, emoji)>`
/// in the same order as `guesses`, ready for posting as a chat message.
///
/// The map auto-fits to contain all points (no manual zoom needed).
pub fn render_round_map(
    guesses:    &[(String, f64, f64)],   // (display_name, lat, lon)
    actual_lat: f64,
    actual_lon: f64,
) -> Option<(Vec<u8>, Vec<(String, &'static str)>)> {
    let mut map = StaticMapBuilder::new()
        .width(700)
        .height(500)
        .padding((40, 40))
        .url_template("https://a.tile.osm.org/{z}/{x}/{y}.png")
        .build()
        .ok()?;

    // ── Lines first so they render behind the circles ─────────────────────────
    for (idx, (_name, lat, lon)) in guesses.iter().enumerate() {
        let (r, g, b, _) = PLAYER_COLORS[idx % PLAYER_COLORS.len()];
        add_line(&mut map, *lat, *lon, actual_lat, actual_lon, r, g, b);
    }

    // ── Player circles ────────────────────────────────────────────────────────
    let mut legend: Vec<(String, &'static str)> = Vec::new();
    for (idx, (name, lat, lon)) in guesses.iter().enumerate() {
        let (r, g, b, emoji) = PLAYER_COLORS[idx % PLAYER_COLORS.len()];
        add_circle(&mut map, *lat, *lon, r, g, b);
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

/// Draw the "actual location" marker: large white ring + small dark fill.
fn add_actual_marker(map: &mut staticmap::StaticMap, lat: f64, lon: f64) {
    if let Ok(ring) = CircleBuilder::new()
        .lat_coordinate(lat)
        .lon_coordinate(lon)
        .color(Color::new(true, 255, 255, 255, 255))
        .radius(11.0)
        .build()
    {
        map.add_tool(ring);
    }
    if let Ok(fill) = CircleBuilder::new()
        .lat_coordinate(lat)
        .lon_coordinate(lon)
        .color(Color::new(true, 220, 50, 50, 255))
        .radius(8.0)
        .build()
    {
        map.add_tool(fill);
    }
}

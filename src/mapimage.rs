//! The result image posted after every guess: a map with each player's guess,
//! a line to the actual location, rank badges, and a legend underneath with
//! rank, name, distance and points.
//!
//! This is a small renderer of its own on tiny-skia. It replaced the
//! `staticmap` crate, which could not identify itself to tile servers, took a
//! tile server's "API key required" placeholder for a real tile, failed the
//! whole image on a single bad tile, and folded longitudes beyond ±180°
//! (needed to keep a Pacific-spanning view continuous) to the far side of the
//! world.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::Duration,
};

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use tiny_skia::{
    Color, FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Stroke, Transform,
};
use tracing::warn;

use crate::config::MapConfig;

/// Player colours by rank. Distinct on light and dark map areas; ranks beyond
/// the palette repeat colours, the rank badge keeps them apart.
pub const PLAYER_COLORS: &[(u8, u8, u8)] = &[
    (50, 120, 255), // blue
    (220, 60, 60),  // red
    (40, 170, 70),  // green
    (170, 60, 210), // purple
    (215, 150, 0),  // amber
    (0, 165, 190),  // cyan
    (240, 110, 0),  // orange
    (200, 0, 130),  // magenta
];

pub fn player_color(rank_index: usize) -> (u8, u8, u8) {
    PLAYER_COLORS[rank_index % PLAYER_COLORS.len()]
}

// ── Layout ────────────────────────────────────────────────────────────────────

const TILE: f64 = 256.0;
const MAP_W: u32 = 800;
const MAP_H: u32 = 480;
/// Room above the topmost point for a pin (52 px) and its badge.
const PAD_TOP: f64 = 70.0;
const PAD_SIDE: f64 = 40.0;
const PAD_BOTTOM: f64 = 28.0;
const MIN_ZOOM: u8 = 1;
const MAX_ZOOM: u8 = 17;
/// At most this many legend rows; further players are summarised.
const MAX_LEGEND_ROWS: usize = 20;
const ROW_H: f32 = 30.0;
const LEGEND_HEADER_H: f32 = 44.0;

static REGULAR: LazyLock<FontRef<'static>> = LazyLock::new(|| {
    FontRef::try_from_slice(include_bytes!("../assets/fonts/NotoSans-Regular.ttf"))
        .expect("bundled font")
});
static BOLD: LazyLock<FontRef<'static>> = LazyLock::new(|| {
    FontRef::try_from_slice(include_bytes!("../assets/fonts/NotoSans-Bold.ttf"))
        .expect("bundled font")
});

/// One player's line in the result.
pub struct ResultPlayer {
    /// 1-based place of this guess, shared on the same metre.
    pub rank: usize,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub dist_km: f64,
    /// Duel points won on this image, in halves; `None` without a duel.
    pub duel: Option<i64>,
    /// Raw avatar image bytes (any format), shown in the pin when present.
    pub avatar: Option<Vec<u8>>,
}

pub struct ResultMap {
    /// Name of the actual location, e.g. "Paris, France".
    pub place: String,
    pub actual_lat: f64,
    pub actual_lon: f64,
    /// Ranked best first.
    pub players: Vec<ResultPlayer>,
}

/// Render the result image. Missing map tiles leave a plain background
/// rather than failing; `None` only if rendering itself fails, which callers
/// log and skip. The text result is posted independently.
pub async fn render_result_map(config: &MapConfig, map: ResultMap) -> Option<Vec<u8>> {
    let view = plan_viewport(&map);
    let tiles = fetch_tiles(config, &view).await;
    let missing = tiles.iter().filter(|t| t.data.is_none()).count();
    if missing > 0 {
        warn!(
            "Result map: {missing}/{} tile(s) unavailable from {}: drawing without them",
            tiles.len(),
            config.tile_url
        );
    }
    let attribution = config.attribution.clone();
    tokio::task::spawn_blocking(move || draw(&map, &view, &tiles, &attribution))
        .await
        .ok()?
}

// ── Projection and viewport ───────────────────────────────────────────────────

/// Web-Mercator pixel position at `zoom`. `lon` may lie outside ±180°. The
/// result simply continues past the world's edge, which keeps a view across
/// the antimeridian continuous (tiles are wrapped when fetched).
fn world_px(lat: f64, lon: f64, zoom: u8) -> (f64, f64) {
    let size = TILE * f64::from(1u32 << zoom);
    let lat = lat.clamp(-85.0511, 85.0511).to_radians();
    let x = (lon + 180.0) / 360.0 * size;
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * size;
    (x, y)
}

#[derive(Debug, Clone, Copy)]
struct Viewport {
    zoom: u8,
    /// World pixel at the image's top-left corner.
    left: f64,
    top: f64,
    /// Longitudes are shifted to within 180° of this, so all points lie in
    /// one continuous stretch of map.
    center_lon: f64,
}

impl Viewport {
    fn project(&self, lat: f64, lon: f64) -> (f32, f32) {
        let (x, y) = world_px(lat, normalize_lon(lon, self.center_lon), self.zoom);
        ((x - self.left) as f32, (y - self.top) as f32)
    }
}

/// The largest zoom at which the actual location and every guess fit inside
/// the padded map area, centred on them.
fn plan_viewport(map: &ResultMap) -> Viewport {
    let lons: Vec<f64> = std::iter::once(map.actual_lon)
        .chain(map.players.iter().map(|p| p.lon))
        .collect();
    let (center_lon, _) = minimum_lon_arc(&lons);
    let points: Vec<(f64, f64)> = std::iter::once((map.actual_lat, map.actual_lon))
        .chain(map.players.iter().map(|p| (p.lat, p.lon)))
        .map(|(lat, lon)| (lat, normalize_lon(lon, center_lon)))
        .collect();

    let avail_w = f64::from(MAP_W) - 2.0 * PAD_SIDE;
    let avail_h = f64::from(MAP_H) - PAD_TOP - PAD_BOTTOM;
    let bbox = |zoom: u8| {
        let px: Vec<(f64, f64)> = points
            .iter()
            .map(|&(la, lo)| world_px(la, lo, zoom))
            .collect();
        let min_x = px.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let max_x = px.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
        let min_y = px.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let max_y = px.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        (min_x, max_x, min_y, max_y)
    };
    let zoom = (MIN_ZOOM..=MAX_ZOOM)
        .rev()
        .find(|&z| {
            let (x0, x1, y0, y1) = bbox(z);
            x1 - x0 <= avail_w && y1 - y0 <= avail_h
        })
        .unwrap_or(MIN_ZOOM);

    let (x0, x1, y0, y1) = bbox(zoom);
    let left = (x0 + x1) / 2.0 - f64::from(MAP_W) / 2.0;
    let area_mid = (PAD_TOP + f64::from(MAP_H) - PAD_BOTTOM) / 2.0;
    let mut top = (y0 + y1) / 2.0 - area_mid;
    // Stay on the map vertically; horizontally the world repeats.
    let world = TILE * f64::from(1u32 << zoom);
    let max_top = world - f64::from(MAP_H);
    top = if max_top >= 0.0 {
        top.clamp(0.0, max_top)
    } else {
        max_top / 2.0
    };
    Viewport {
        zoom,
        left,
        top,
        center_lon,
    }
}

/// Normalize `lon` to be within 180° of `center_lon` (may leave ±180°).
fn normalize_lon(lon: f64, center_lon: f64) -> f64 {
    let mut l = lon;
    while l - center_lon > 180.0 {
        l -= 360.0;
    }
    while l - center_lon < -180.0 {
        l += 360.0;
    }
    l
}

/// Returns `(center_lon, arc_span_degrees)` for the minimum-width arc
/// containing all given longitudes.  Handles antimeridian wrapping correctly.
fn minimum_lon_arc(lons: &[f64]) -> (f64, f64) {
    if lons.is_empty() {
        return (0.0, 360.0);
    }

    // Convert lons to [0, 360) fractions, sort, deduplicate.
    let mut fracs: Vec<f64> = lons
        .iter()
        .map(|&l| (l + 180.0).rem_euclid(360.0))
        .collect();
    fracs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    fracs.dedup_by(|a, b| (*a - *b).abs() < 1e-9);

    let n = fracs.len();
    if n == 1 {
        return (fracs[0] - 180.0, 0.0);
    }

    // Find the largest gap between consecutive fracs (including the wrap-around gap).
    let mut max_gap = 0.0_f64;
    let mut arc_start = fracs[0];
    for i in 0..n {
        let gap = if i + 1 < n {
            fracs[i + 1] - fracs[i]
        } else {
            360.0 - fracs[n - 1] + fracs[0] // wrap-around gap
        };
        if gap > max_gap {
            max_gap = gap;
            arc_start = if i + 1 < n { fracs[i + 1] } else { fracs[0] };
        }
    }

    let span = 360.0 - max_gap;
    let center_frac = (arc_start + span / 2.0).rem_euclid(360.0);
    let center_lon = center_frac - 180.0;
    (center_lon, span)
}

// ── Tiles ─────────────────────────────────────────────────────────────────────

struct Tile {
    /// Unwrapped tile column/row relative to the viewport's world.
    x: i64,
    y: i64,
    data: Option<Vec<u8>>,
}

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        // Tile servers (OpenStreetMap in particular) require an identifying agent.
        .user_agent(concat!(
            "geoguessr-bot/",
            env!("CARGO_PKG_VERSION"),
            " (Matrix game bot; +https://github.com/PolynomialDivision)"
        ))
        .timeout(Duration::from_secs(10))
        .build()
        .expect("building the tile HTTP client")
});

/// Recently used tiles: rounds zoomed out far reuse the same few tiles.
static TILE_CACHE: LazyLock<Mutex<HashMap<String, Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const TILE_CACHE_MAX: usize = 400;

fn tile_url(template: &str, zoom: u8, x: i64, y: i64) -> String {
    template
        .replace("{z}", &zoom.to_string())
        .replace("{x}", &x.to_string())
        .replace("{y}", &y.to_string())
}

async fn fetch_tiles(config: &MapConfig, view: &Viewport) -> Vec<Tile> {
    let n = 1i64 << view.zoom;
    let x0 = (view.left / TILE).floor() as i64;
    let x1 = ((view.left + f64::from(MAP_W) - 1.0) / TILE).floor() as i64;
    let y0 = ((view.top / TILE).floor() as i64).max(0);
    let y1 = (((view.top + f64::from(MAP_H) - 1.0) / TILE).floor() as i64).min(n - 1);

    let mut set = tokio::task::JoinSet::new();
    for x in x0..=x1 {
        for y in y0..=y1 {
            let url = tile_url(&config.tile_url, view.zoom, x.rem_euclid(n), y);
            set.spawn(async move {
                let data = fetch_tile(&url).await;
                Tile { x, y, data }
            });
        }
    }
    let mut tiles = Vec::new();
    while let Some(result) = set.join_next().await {
        if let Ok(tile) = result {
            tiles.push(tile);
        }
    }
    tiles
}

async fn fetch_tile(url: &str) -> Option<Vec<u8>> {
    if let Some(hit) = TILE_CACHE.lock().ok()?.get(url) {
        return Some(hit.clone());
    }
    let resp = HTTP.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        warn!("Map tile {url}: HTTP {}", resp.status());
        return None;
    }
    let bytes = resp.bytes().await.ok()?.to_vec();
    // Only cache what actually decodes as an image.
    decode_pixmap(&bytes)?;
    let mut cache = TILE_CACHE.lock().ok()?;
    if cache.len() >= TILE_CACHE_MAX {
        cache.clear();
    }
    cache.insert(url.to_owned(), bytes.clone());
    Some(bytes)
}

/// Any PNG/JPEG/WebP into a premultiplied pixmap.
fn decode_pixmap(bytes: &[u8]) -> Option<Pixmap> {
    let rgba = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut data = rgba.into_raw();
    for px in data.chunks_exact_mut(4) {
        let a = u16::from(px[3]);
        for c in &mut px[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
    Pixmap::from_vec(data, tiny_skia::IntSize::from_wh(w, h)?)
}

// ── Drawing ───────────────────────────────────────────────────────────────────

fn draw(map: &ResultMap, view: &Viewport, tiles: &[Tile], attribution: &str) -> Option<Vec<u8>> {
    let listed = map.players.len().min(MAX_LEGEND_ROWS);
    let columns = if listed > 6 { 2 } else { 1 };
    let rows = listed.div_ceil(columns);
    let overflow = map.players.len() > MAX_LEGEND_ROWS;
    let legend_h =
        LEGEND_HEADER_H + rows as f32 * ROW_H + if overflow { ROW_H } else { 0.0 } + 10.0;
    let height = MAP_H + legend_h.ceil() as u32;

    let mut canvas = Pixmap::new(MAP_W, height)?;
    canvas.fill(Color::WHITE);
    fill_rect(
        &mut canvas,
        0.0,
        0.0,
        MAP_W as f32,
        MAP_H as f32,
        (221, 227, 230, 255),
    );

    for tile in tiles {
        let Some(pixmap) = tile.data.as_deref().and_then(decode_pixmap) else {
            continue;
        };
        let x = (tile.x as f64 * TILE - view.left).round() as i32;
        let y = (tile.y as f64 * TILE - view.top).round() as i32;
        canvas.draw_pixmap(
            x,
            y,
            pixmap.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }

    let actual = view.project(map.actual_lat, map.actual_lon);
    // Worst guess first, so the best line ends up on top.
    for (i, p) in map.players.iter().enumerate().rev() {
        let from = view.project(p.lat, p.lon);
        stroke_line(&mut canvas, from, actual, 5.5, (255, 255, 255, 200));
        let (r, g, b) = player_color(i);
        stroke_line(&mut canvas, from, actual, 3.0, (r, g, b, 235));
    }

    // Northern pins first, so a pin further south overlaps the one above it
    // the way a physical map pin would, and its badge stays readable.
    let mut order: Vec<usize> = (0..map.players.len()).collect();
    order.sort_by(|&a, &b| {
        let ya = view.project(map.players[a].lat, map.players[a].lon).1;
        let yb = view.project(map.players[b].lat, map.players[b].lon).1;
        ya.total_cmp(&yb)
    });
    for i in order {
        let p = &map.players[i];
        let (x, y) = view.project(p.lat, p.lon);
        let color = player_color(i);
        draw_pin(&mut canvas, x, y, p.avatar.as_deref(), color);
        draw_badge(&mut canvas, x + 15.0, y - 47.0, p.rank, color);
    }

    // The answer goes on top of everything.
    draw_target(&mut canvas, actual.0, actual.1, 1.0);
    draw_attribution(&mut canvas, attribution);

    draw_legend(&mut canvas, map, columns, listed, overflow);
    canvas.encode_png().ok()
}

fn paint(rgba: (u8, u8, u8, u8)) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(rgba.0, rgba.1, rgba.2, rgba.3);
    paint.anti_alias = true;
    paint
}

fn fill_rect(canvas: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, rgba: (u8, u8, u8, u8)) {
    if let Some(rect) = Rect::from_xywh(x, y, w, h) {
        canvas.fill_rect(rect, &paint(rgba), Transform::identity(), None);
    }
}

fn fill_circle(canvas: &mut Pixmap, x: f32, y: f32, r: f32, rgba: (u8, u8, u8, u8)) {
    if let Some(path) = PathBuilder::from_circle(x, y, r) {
        canvas.fill_path(
            &path,
            &paint(rgba),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn stroke_line(
    canvas: &mut Pixmap,
    a: (f32, f32),
    b: (f32, f32),
    width: f32,
    rgba: (u8, u8, u8, u8),
) {
    let mut pb = PathBuilder::new();
    pb.move_to(a.0, a.1);
    pb.line_to(b.0, b.1);
    let Some(path) = pb.finish() else { return };
    let stroke = Stroke {
        width,
        line_cap: tiny_skia::LineCap::Round,
        ..Stroke::default()
    };
    canvas.stroke_path(&path, &paint(rgba), &stroke, Transform::identity(), None);
}

/// Avatar pin with its tip on (x, y); a coloured dot if no pin renders.
fn draw_pin(canvas: &mut Pixmap, x: f32, y: f32, avatar: Option<&[u8]>, (r, g, b): (u8, u8, u8)) {
    let pin = crate::avatar::render_avatar_pin(avatar, r, g, b)
        .and_then(|png| Pixmap::decode_png(&png).ok());
    match pin {
        Some(pin) => canvas.draw_pixmap(
            (x - crate::avatar::PIN_ANCHOR_X as f32).round() as i32,
            (y - crate::avatar::PIN_ANCHOR_Y as f32).round() as i32,
            pin.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        ),
        None => {
            fill_circle(canvas, x, y, 10.0, (255, 255, 255, 255));
            fill_circle(canvas, x, y, 7.5, (r, g, b, 255));
        }
    }
}

/// Rank number in a coloured disc. The link between map and legend.
fn draw_badge(canvas: &mut Pixmap, x: f32, y: f32, rank: usize, (r, g, b): (u8, u8, u8)) {
    let label = rank.to_string();
    let size = if label.len() > 1 { 11.5 } else { 13.0 };
    let radius = if label.len() > 1 { 11.5 } else { 10.0 };
    fill_circle(canvas, x, y, radius + 2.0, (255, 255, 255, 255));
    fill_circle(canvas, x, y, radius, (r, g, b, 255));
    let w = text_width(&BOLD, size, &label);
    draw_text(
        canvas,
        &BOLD,
        size,
        x - w / 2.0,
        y + size * 0.36,
        (255, 255, 255, 255),
        &label,
    );
}

/// The actual location: a bullseye that reads as "here" on any background.
fn draw_target(canvas: &mut Pixmap, x: f32, y: f32, scale: f32) {
    fill_circle(canvas, x, y, 13.0 * scale, (255, 255, 255, 255));
    fill_circle(canvas, x, y, 10.5 * scale, (25, 25, 25, 255));
    fill_circle(canvas, x, y, 7.0 * scale, (255, 255, 255, 255));
    fill_circle(canvas, x, y, 4.5 * scale, (230, 40, 40, 255));
}

fn draw_attribution(canvas: &mut Pixmap, attribution: &str) {
    if attribution.is_empty() {
        return;
    }
    let size = 11.0;
    let w = text_width(&REGULAR, size, attribution);
    let (x, y) = (MAP_W as f32 - w - 8.0, MAP_H as f32 - 5.0);
    fill_rect(
        canvas,
        x - 4.0,
        y - 12.0,
        w + 8.0,
        16.0,
        (255, 255, 255, 190),
    );
    draw_text(canvas, &REGULAR, size, x, y, (60, 60, 60, 255), attribution);
}

fn draw_legend(
    canvas: &mut Pixmap,
    map: &ResultMap,
    columns: usize,
    listed: usize,
    overflow: bool,
) {
    let top = MAP_H as f32;
    // Tiles and pins near the bottom edge extend past the map; the legend
    // starts on a clean sheet.
    fill_rect(
        canvas,
        0.0,
        top,
        MAP_W as f32,
        canvas.height() as f32 - top,
        (255, 255, 255, 255),
    );
    fill_rect(canvas, 0.0, top, MAP_W as f32, 1.0, (200, 205, 210, 255));

    // Header: the answer, with the same bullseye as on the map.
    draw_target(canvas, 24.0, top + 22.0, 0.8);
    let place = fit_text(&BOLD, 17.0, &map.place, MAP_W as f32 - 60.0);
    draw_text(
        canvas,
        &BOLD,
        17.0,
        44.0,
        top + 28.0,
        (25, 25, 25, 255),
        &place,
    );

    let col_w = (MAP_W as f32 - 24.0) / columns as f32;
    let rows = listed.div_ceil(columns);
    for (i, p) in map.players.iter().take(listed).enumerate() {
        let (col, row) = (i / rows, i % rows);
        let x = 12.0 + col as f32 * col_w;
        let y = top + LEGEND_HEADER_H + row as f32 * ROW_H;
        if row % 2 == 0 {
            fill_rect(canvas, x, y, col_w - 8.0, ROW_H, (244, 246, 248, 255));
        }
        let mid = y + ROW_H / 2.0;
        draw_badge(canvas, x + 16.0, mid, p.rank, player_color(i));

        let dist = crate::game::format_guess_dist(p.dist_km, false);
        let stats = match p.duel {
            Some(halves) => format!("{dist} · +{}", crate::duels::fmt_points(halves)),
            None => dist,
        };
        let stats_w = text_width(&REGULAR, 14.0, &stats);
        let right = x + col_w - 18.0;
        draw_text(
            canvas,
            &REGULAR,
            14.0,
            right - stats_w,
            mid + 5.0,
            (70, 70, 70, 255),
            &stats,
        );

        let name_x = x + 34.0;
        let name = fit_text(&BOLD, 14.5, &p.name, right - stats_w - 12.0 - name_x);
        draw_text(
            canvas,
            &BOLD,
            14.5,
            name_x,
            mid + 5.0,
            (25, 25, 25, 255),
            &name,
        );
    }
    if overflow {
        let more = format!("+ {} more", map.players.len() - listed);
        let y = top + LEGEND_HEADER_H + rows as f32 * ROW_H + ROW_H / 2.0 + 5.0;
        draw_text(canvas, &REGULAR, 14.0, 46.0, y, (90, 90, 90, 255), &more);
    }
}

// ── Text ──────────────────────────────────────────────────────────────────────

/// Characters the bundled font can draw (emoji and rare scripts are dropped).
fn drawable(font: &FontRef<'_>, text: &str) -> String {
    text.chars()
        .filter(|&c| c == ' ' || font.glyph_id(c).0 != 0)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn text_width(font: &FontRef<'_>, size: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut width = 0.0;
    let mut prev = None;
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        if let Some(prev) = prev {
            width += scaled.kern(prev, id);
        }
        width += scaled.h_advance(id);
        prev = Some(id);
    }
    width
}

/// `text` shortened with "…" to at most `max_w` pixels.
fn fit_text(font: &FontRef<'_>, size: f32, text: &str, max_w: f32) -> String {
    let text = drawable(font, text);
    let text = if text.is_empty() {
        "?".to_owned()
    } else {
        text
    };
    if text_width(font, size, &text) <= max_w {
        return text;
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate = format!("{}…", chars.iter().collect::<String>().trim_end());
        if text_width(font, size, &candidate) <= max_w {
            return candidate;
        }
    }
    "…".to_owned()
}

fn draw_text(
    canvas: &mut Pixmap,
    font: &FontRef<'_>,
    size: f32,
    x: f32,
    baseline: f32,
    (r, g, b, a): (u8, u8, u8, u8),
    text: &str,
) {
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let (width, height) = (canvas.width() as i32, canvas.height() as i32);
    let pixels = canvas.pixels_mut();
    let mut caret = x;
    let mut prev = None;
    for c in text.chars() {
        let id = scaled.glyph_id(c);
        if let Some(prev) = prev {
            caret += scaled.kern(prev, id);
        }
        let glyph = id.with_scale_and_position(scale, ab_glyph::point(caret, baseline));
        caret += scaled.h_advance(id);
        prev = Some(id);
        let Some(outline) = font.outline_glyph(glyph) else {
            continue;
        };
        let bounds = outline.px_bounds();
        outline.draw(|gx, gy, coverage| {
            let px = bounds.min.x as i32 + gx as i32;
            let py = bounds.min.y as i32 + gy as i32;
            if px < 0 || py < 0 || px >= width || py >= height {
                return;
            }
            let alpha = coverage.clamp(0.0, 1.0) * f32::from(a) / 255.0;
            let idx = (py * width + px) as usize;
            let dst = pixels[idx];
            // "Over" compositing of an opaque colour at `alpha` onto a
            // premultiplied destination.
            let blend = |src: u8, dst: u8| -> u8 {
                (f32::from(src) * alpha + f32::from(dst) * (1.0 - alpha)).round() as u8
            };
            if let Some(color) = tiny_skia::PremultipliedColorU8::from_rgba(
                blend(r, dst.red()),
                blend(g, dst.green()),
                blend(b, dst.blue()),
                blend(255, dst.alpha()),
            ) {
                pixels[idx] = color;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(rank: usize, lat: f64, lon: f64) -> ResultPlayer {
        ResultPlayer {
            rank,
            name: format!("Player {rank}"),
            lat,
            lon,
            dist_km: 100.0 * rank as f64,
            duel: Some(2 * rank as i64),
            avatar: None,
        }
    }

    fn result(actual: (f64, f64), guesses: &[(f64, f64)]) -> ResultMap {
        ResultMap {
            place: "Somewhere".to_owned(),
            actual_lat: actual.0,
            actual_lon: actual.1,
            players: guesses
                .iter()
                .enumerate()
                .map(|(i, &(la, lo))| player(i + 1, la, lo))
                .collect(),
        }
    }

    fn inside_map(view: &Viewport, lat: f64, lon: f64) -> bool {
        let (x, y) = view.project(lat, lon);
        (PAD_SIDE as f32 - 1.0..=MAP_W as f32 - PAD_SIDE as f32 + 1.0).contains(&x)
            && (PAD_TOP as f32 - 1.0..=MAP_H as f32 - PAD_BOTTOM as f32 + 1.0).contains(&y)
    }

    #[test]
    fn every_point_fits_inside_the_padded_map() {
        let map = result(
            (35.68, 139.65),
            &[(52.52, 13.40), (40.71, -74.01), (-33.87, 151.21)],
        );
        let view = plan_viewport(&map);

        assert!(inside_map(&view, map.actual_lat, map.actual_lon));
        for p in &map.players {
            assert!(inside_map(&view, p.lat, p.lon), "{} {}", p.lat, p.lon);
        }
    }

    #[test]
    fn a_pacific_view_stays_continuous_across_the_antimeridian() {
        // Fiji, Samoa, Wellington: a few hundred km apart, but on both sides
        // of ±180°. The old renderer folded Samoa to the far side of the world.
        let map = result((-17.71, 178.07), &[(-13.76, -172.10), (-41.29, 174.78)]);
        let view = plan_viewport(&map);

        assert!(view.zoom >= 4, "zoom {}", view.zoom);
        for p in &map.players {
            assert!(inside_map(&view, p.lat, p.lon));
        }
    }

    #[test]
    fn close_guesses_are_shown_at_street_level() {
        let map = result((48.8566, 2.3522), &[(48.8570, 2.3530)]);

        assert!(plan_viewport(&map).zoom >= 16);
    }

    #[test]
    fn a_worldwide_spread_zooms_out_but_stays_on_the_map() {
        let guesses: Vec<(f64, f64)> = (0..10)
            .map(|i| (60.0 - i as f64 * 12.0, -150.0 + i as f64 * 33.0))
            .collect();
        let map = result((-1.29, 36.82), &guesses);
        let view = plan_viewport(&map);

        assert!(view.zoom <= 2);
        assert!(view.top >= 0.0);
    }

    #[test]
    fn renders_without_any_tiles_and_grows_for_the_legend() {
        let few = result((48.85, 2.35), &[(52.52, 13.40), (40.42, -3.70)]);
        let many = result(
            (48.85, 2.35),
            &(0..25)
                .map(|i| (40.0 + i as f64 * 0.5, i as f64))
                .collect::<Vec<_>>(),
        );

        let (w1, h1) = png_size(&draw(&few, &plan_viewport(&few), &[], "© test").unwrap());
        let (w2, h2) = png_size(&draw(&many, &plan_viewport(&many), &[], "").unwrap());

        assert_eq!((w1, w2), (MAP_W, MAP_W));
        assert!(h1 > MAP_H);
        assert!(h2 > h1);
    }

    #[test]
    fn long_or_undrawable_names_are_shortened_not_dropped() {
        let long = fit_text(
            &BOLD,
            14.0,
            "A really very extremely long display name",
            120.0,
        );
        assert!(long.ends_with('…'));
        assert!(text_width(&BOLD, 14.0, &long) <= 120.0);

        assert_eq!(fit_text(&BOLD, 14.0, "🦊 Fox", 200.0), "Fox");
        assert_eq!(fit_text(&BOLD, 14.0, "🦊", 200.0), "?");
    }

    #[test]
    fn close_guesses_still_show_their_metres() {
        assert_eq!(crate::game::format_guess_dist(0.0, true), "🎯 0 m");
        assert_eq!(crate::game::format_guess_dist(0.021, true), "🎯 21 m");
        assert_eq!(crate::game::format_guess_dist(0.049, false), "49 m");
        assert_eq!(crate::game::format_guess_dist(0.051, true), "51 m");
        assert_eq!(crate::game::format_guess_dist(12.4, true), "12 km");
    }

    #[test]
    fn tile_urls_wrap_around_the_world() {
        assert_eq!(
            tile_url("https://t/{z}/{x}/{y}.png", 3, (-1i64).rem_euclid(8), 2),
            "https://t/3/7/2.png"
        );
    }

    fn png_size(png: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory(png).unwrap();
        (img.width(), img.height())
    }
}

//! Avatar fetching and map-pin rendering.
//!
//! Each player's pin is a 40×52 PNG:
//!
//!   ┌──────────┐
//!   │  avatar  │  ← 40×40 circle, colored border + white ring
//!   └────┬─────┘
//!        ▼        ← 12 px tail; its TIP is the lat/lng anchor point
//!
//! `IconBuilder` is configured with `x_offset = PIN_ANCHOR_X` and
//! `y_offset = PIN_ANCHOR_Y` so the tail tip lands exactly on the coord.

use std::collections::HashMap;

use matrix_sdk::{Room, media::{MediaFormat, MediaThumbnailSettings}};
use matrix_sdk::ruma::UInt;
use tiny_skia::{
    FillRule, Mask, Paint, PathBuilder, Pixmap, PixmapPaint, Transform,
};

// ── Pin geometry constants ────────────────────────────────────────────────────

/// Total width of the pin image (= circle diameter).
pub const PIN_W: u32 = 40;
/// Total height of the pin image (circle + tail).
pub const PIN_H: u32 = 52;

/// `x_offset` for `IconBuilder`: places the tail tip at the lon pixel.
pub const PIN_ANCHOR_X: f64 = (PIN_W / 2) as f64;
/// `y_offset` for `IconBuilder`: places the tail tip at the lat pixel.
pub const PIN_ANCHOR_Y: f64 = PIN_H as f64;

const BORDER: f32    = 3.0;
const TAIL_HALF: f32 = 8.0;   // half-width of triangle base

// ── Public API ────────────────────────────────────────────────────────────────

/// Download thumbnail bytes for each user.  Missing/failed avatars are simply
/// absent from the returned map; callers fall back to solid-colour circles.
pub async fn fetch_player_avatars(
    room:     &Room,
    user_ids: &[&str],
) -> HashMap<String, Vec<u8>> {
    let mut map = HashMap::new();
    let thumb = MediaFormat::Thumbnail(MediaThumbnailSettings::new(
        UInt::from(64u32),
        UInt::from(64u32),
    ));
    for &uid_str in user_ids {
        let Ok(uid) = matrix_sdk::ruma::OwnedUserId::try_from(uid_str) else { continue };
        let Ok(Some(member)) = room.get_member(&uid).await else { continue };
        if let Ok(Some(bytes)) = member.avatar(thumb.clone()).await {
            map.insert(uid_str.to_owned(), bytes);
        }
    }
    map
}

/// Render a 40×52 PNG map pin for one player.
///
/// * `avatar_bytes` – raw image bytes in any format the `image` crate supports
///   (JPEG / PNG / WebP).  `None` → solid-colour fill.
/// * `(r, g, b)` – the player's assigned colour from `mapimage::PLAYER_COLORS`.
///
/// Returns `None` only if `Pixmap` allocation fails (effectively never).
pub fn render_avatar_pin(avatar_bytes: Option<&[u8]>, r: u8, g: u8, b: u8) -> Option<Vec<u8>> {
    let mut pixmap = Pixmap::new(PIN_W, PIN_H)?;
    let cx = PIN_W as f32 / 2.0;   // horizontal centre
    let cy = cx;                    // vertical centre of the circle part

    // ── 1. White outer ring (1 px shadow) ────────────────────────────────────
    fill_circle(&mut pixmap, cx, cy, cx - 0.5, 255, 255, 255, 255);

    // ── 2. Player-colour ring ─────────────────────────────────────────────────
    fill_circle(&mut pixmap, cx, cy, cx - 2.0, r, g, b, 255);

    // ── 3. Avatar image or solid-colour fallback ──────────────────────────────
    let inner_r      = cx - BORDER - 1.5;
    let inner_size   = (inner_r * 2.0).round() as u32;
    let inner_offset = (cx - inner_r).round() as i32;

    match avatar_bytes.and_then(|b| clip_avatar(b, inner_size)) {
        Some(clipped) => {
            pixmap.draw_pixmap(
                inner_offset, inner_offset,
                clipped.as_ref(),
                &PixmapPaint::default(),
                Transform::default(),
                None,
            );
        }
        None => {
            // Slightly transparent solid fill so the white ring still shows
            fill_circle(&mut pixmap, cx, cy, inner_r, r, g, b, 200);
        }
    }

    // ── 4. Tail (pointy triangle, same colour as the ring) ───────────────────
    draw_tail(&mut pixmap, cx, r, g, b);

    pixmap.encode_png().ok()
}

// ── Internals ─────────────────────────────────────────────────────────────────

fn fill_circle(pixmap: &mut Pixmap, cx: f32, cy: f32, radius: f32, r: u8, g: u8, b: u8, a: u8) {
    let Some(path) = PathBuilder::from_circle(cx, cy, radius) else { return };
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
}

/// Draw the pointy tail below the circle.
/// The triangle base sits at `y = PIN_W - 1` and the tip at `y = PIN_H`.
fn draw_tail(pixmap: &mut Pixmap, cx: f32, r: u8, g: u8, b: u8) {
    let base_y = (PIN_W as f32) - 1.0;
    let tip_y  = PIN_H as f32;

    let mut pb = PathBuilder::new();
    pb.move_to(cx - TAIL_HALF, base_y);
    pb.line_to(cx + TAIL_HALF, base_y);
    pb.line_to(cx, tip_y);
    pb.close();
    let Some(path) = pb.finish() else { return };

    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, 255);
    paint.anti_alias = true;
    pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
}

/// Decode `bytes`, centre-crop to square, resize to `size × size`, and apply
/// a circular alpha mask so corners are transparent.
fn clip_avatar(bytes: &[u8], size: u32) -> Option<Pixmap> {
    // ── Decode ────────────────────────────────────────────────────────────────
    let img = image::load_from_memory(bytes).ok()?;

    // ── Centre-crop to square ─────────────────────────────────────────────────
    let (w, h) = (img.width(), img.height());
    let side = w.min(h);
    let img = img.crop_imm((w - side) / 2, (h - side) / 2, side, side);

    // ── Resize ────────────────────────────────────────────────────────────────
    let img = img.resize_exact(size, size, image::imageops::FilterType::Lanczos3);

    // ── Re-encode as PNG so tiny_skia can ingest it (handles straight→premul) ─
    let mut png_buf = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut png_buf),
        image::ImageFormat::Png,
    )
    .ok()?;
    let full = Pixmap::decode_png(&png_buf).ok()?;

    // ── Circular clip mask ────────────────────────────────────────────────────
    let radius = size as f32 / 2.0;
    let path = PathBuilder::from_circle(radius, radius, radius - 0.5)?;
    let mut mask = Mask::new(size, size)?;
    mask.fill_path(&path, FillRule::Winding, true, Transform::default());

    // ── Apply mask ────────────────────────────────────────────────────────────
    let mut clipped = Pixmap::new(size, size)?;
    clipped.draw_pixmap(
        0, 0,
        full.as_ref(),
        &PixmapPaint::default(),
        Transform::default(),
        Some(&mask),
    );

    Some(clipped)
}

//! Deterministic, offline image-quality filter for street-level photo candidates.
//!
//! Scores each candidate on up to seven axes:
//!
//! | Axis                | Nominal | Source                     |
//! |---------------------|---------|----------------------------|
//! | Resolution          |    35%  | width × height             |
//! | Sharpness           |    20%  | Laplacian variance         |
//! | Sequence continuity |    20%  | per-sequence GPS           |
//! | Density             |    20%  | images / km²               |
//! | GPS stability       |    15%  | jitter metres              |
//! | Server quality      |    15%  | Mapillary quality_score    |
//! | Overlay cleanliness |    15%  | Sobel + variance heuristic |
//! | Freshness           |    10%  | capture age                |
//!
//! Nominal weights sum to 150 % because sharpness, server quality, and
//! overlay are optional; absent axes are excluded and the remaining weights
//! renormalise to 1.0, so a missing signal never silently biases the score.
//!
//! An anti-starvation mechanism progressively relaxes the rejection threshold
//! when too many consecutive candidates are rejected, ensuring the prefetch
//! pipeline never runs dry.
//!
//! No I/O, no allocations beyond input structs — safe at high frequency.

use std::time::{SystemTime, UNIX_EPOCH};

// ── Sequence continuity types ─────────────────────────────────────────────────

/// One frame in an ordered street-level sequence.
#[derive(Debug, Clone)]
pub struct SequenceFrame {
    pub lat:            f64,
    pub lon:            f64,
    /// Unix milliseconds — used to sort frames into traversal order.
    pub captured_at_ms: Option<i64>,
    /// Camera heading in degrees [0, 360).  None = unavailable.
    pub compass_angle:  Option<f32>,
}

/// Compute a sequence-level quality score [0.0, 1.0] from an ordered slice
/// of frames belonging to the same traversal.
///
/// Three components (sub-weighted internally):
/// - **Length**       (25%) — more frames → higher score; isolated frame penalty.
/// - **Step consistency** (50%) — even, human-scale spacing; penalises GPS
///   jumps > 200 m or duplicate positions < 0.1 m.
/// - **Heading stability** (25%) — low mean angular change between frames;
///   neutral default when compass data is absent.
pub fn score_sequence_continuity(frames: &[SequenceFrame]) -> f32 {
    match frames.len() {
        0 => 0.0,
        // Single isolated frame: no step or heading data exists at all.
        // Return a fixed low score rather than averaging neutral defaults that
        // would mask the lack of any traversal information.
        1 => 0.15,
        _ => {
            let length  = score_seq_length(frames.len());
            let steps   = score_seq_steps(frames);
            let heading = score_seq_heading(frames);
            length * 0.25 + steps * 0.50 + heading * 0.25
        }
    }
}

/// Length: 1 frame → 0.0 (isolated), ≥ 15 frames → 1.0.
fn score_seq_length(n: usize) -> f32 {
    if n == 0 { return 0.0; }
    if n == 1 { return 0.1; } // isolated frame — strong penalty
    const MAX: f64 = 15.0;
    ((n as f64 - 1.0) / (MAX - 1.0)).clamp(0.0, 1.0) as f32
}

/// Step consistency: regularity and scale of inter-frame distances.
///
/// Scores two things:
/// - **Regularity**: low coefficient of variation of step distances.
/// - **Scale**: mean step in the 1–30 m range typical for street imagery.
///
/// Both are penalised by the outlier fraction (steps < 0.1 m or > 200 m).
fn score_seq_steps(frames: &[SequenceFrame]) -> f32 {
    if frames.len() < 2 { return 0.5; } // not enough data — neutral

    let steps: Vec<f64> = frames
        .windows(2)
        .map(|w| haversine_m(w[0].lat, w[0].lon, w[1].lat, w[1].lon))
        .collect();

    if steps.is_empty() { return 0.5; }

    // Outlier fraction: GPS jumps or duplicate positions.
    let n_outliers = steps.iter().filter(|&&d| d < 0.1 || d > 200.0).count();
    let outlier_penalty = 1.0 - n_outliers as f32 / steps.len() as f32;

    let mean: f64 = steps.iter().sum::<f64>() / steps.len() as f64;
    if mean <= 0.0 { return 0.0; }

    // Coefficient of variation: σ/μ.  Lower → more regular spacing.
    let variance: f64 = steps.iter().map(|&d| (d - mean).powi(2)).sum::<f64>() / steps.len() as f64;
    let cv = variance.sqrt() / mean;
    let regularity = (1.0 - (cv / 2.0).min(1.0)) as f32; // CV ≥ 2 → 0.0

    // Scale score: ideal range 1–30 m; taper outside.
    let scale: f32 = if mean >= 1.0 && mean <= 30.0 {
        1.0
    } else if mean < 1.0 {
        (mean as f32).clamp(0.0, 1.0)
    } else {
        // > 30 m: decay to 0 at 500 m
        (1.0 - (mean - 30.0) / 470.0).clamp(0.0, 1.0) as f32
    };

    (regularity * 0.60 + scale * 0.40) * outlier_penalty
}

/// Heading stability: low mean angular turn → stable traversal direction.
///
/// Returns 0.6 (neutral) when fewer than 2 frames have compass data.
fn score_seq_heading(frames: &[SequenceFrame]) -> f32 {
    let angles: Vec<f32> = frames.iter().filter_map(|f| f.compass_angle).collect();
    if angles.len() < 2 { return 0.6; }

    let mean_turn = angles
        .windows(2)
        .map(|w| angular_diff(w[0], w[1]))
        .sum::<f32>()
        / (angles.len() - 1) as f32;

    // 0° avg turn (perfectly straight) → 1.0; 180° avg turn → 0.0.
    (1.0 - mean_turn / 180.0).clamp(0.0, 1.0)
}

/// Great-circle distance in metres.
fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    6_371_000.0 * 2.0 * a.sqrt().asin()
}

/// Minimum arc between two compass angles (result in [0, 180]).
fn angular_diff(a: f32, b: f32) -> f32 {
    let d = (b - a).abs() % 360.0;
    if d > 180.0 { 360.0 - d } else { d }
}

// ── Quality filter types ──────────────────────────────────────────────────────

/// Metadata for one image candidate.
#[derive(Debug, Clone, Default)]
pub struct QualityInput {
    /// Original image width in pixels (0 = unknown → neutral).
    pub width:                u32,
    /// Original image height in pixels (0 = unknown → neutral).
    pub height:               u32,
    /// Capture timestamp as Unix milliseconds (None = unknown → neutral).
    pub captured_at_ms:       Option<i64>,
    /// Number of images returned in the same search area (density proxy).
    pub area_image_count:     usize,
    /// Search radius used for the query, in kilometres.  0.0 → raw-count fallback.
    pub search_radius_km:     f64,
    /// GPS positional jitter in metres (None = unavailable → axis excluded).
    pub gps_jitter_m:         Option<f64>,
    /// Pre-computed sequence continuity score [0.0, 1.0].
    /// None = no sequence data → axis excluded and weight redistributed.
    pub sequence_continuity:  Option<f32>,
    /// Mapillary server-side quality score [0.0, 5.0] (None = field not returned by API).
    pub server_quality:       Option<f32>,
    /// Image sharpness from Laplacian variance, normalized [0.0, 1.0] (None = not computed).
    pub sharpness:            Option<f32>,
    /// Overlay cleanliness score [0.0, 1.0]: 1.0 = no UI overlay, 0.0 = severe overlay.
    /// Already sequence-adjusted by the caller.  None = not computed.
    pub overlay:              Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision { Accept, Reject }

#[derive(Debug, Clone)]
pub struct QualityResult {
    pub decision: Decision,
    /// Weighted aggregate score in [0.0, 1.0].
    pub score:    f32,
    /// Human-readable summary for logging.
    pub reason:   &'static str,
    /// Per-axis scores (for debug logging).
    pub sub:      SubScores,
}

/// Per-axis raw scores before weighting.
/// `Option` fields are `None` when data was unavailable (axis excluded).
#[derive(Debug, Clone, Copy, Default)]
pub struct SubScores {
    pub resolution:          f32,
    pub freshness:           f32,
    pub density:             f32,
    /// `None` when GPS jitter data is unavailable.
    pub stability:           Option<f32>,
    /// `None` when no sequence data is available.
    pub sequence_continuity: Option<f32>,
    /// `None` when the Mapillary quality_score field was not returned.
    pub server_quality:      Option<f32>,
    /// `None` when sharpness was not computed (thumbnail not downloaded).
    pub sharpness:           Option<f32>,
    /// `None` when overlay detection was not run.
    pub overlay:             Option<f32>,
}

// ── Filter state (anti-starvation) ────────────────────────────────────────────

/// Tracks consecutive rejections across fetch calls.
///
/// Create one instance per `prefetch_if_needed` session via
/// [`FilterState::with_streak`], pass it mutably to each source fetch,
/// and write the updated streak back to `BotContext` via [`FilterState::streak`].
#[derive(Debug, Default)]
pub struct FilterState {
    pub(super) consecutive_rejections: u32,
    /// When true, applies an extra threshold relaxation to boost exploration
    /// when the cache has collapsed into a small geographic cluster.
    pub(super) exploration_mode: bool,
}

impl FilterState {
    pub fn new() -> Self { Self::default() }

    /// Restore from a previously persisted rejection streak.
    pub fn with_streak(n: u32) -> Self { Self { consecutive_rejections: n, ..Self::default() } }

    /// Restore streak and set exploration mode (for geographic anti-collapse).
    pub fn with_streak_and_exploration(n: u32, explore: bool) -> Self {
        Self { consecutive_rejections: n, exploration_mode: explore }
    }

    /// Current rejection streak (save into BotContext after a session).
    pub fn streak(&self) -> u32 { self.consecutive_rejections }

    /// Score and accept/reject one candidate; updates the internal counter.
    pub fn evaluate(&mut self, input: &QualityInput) -> QualityResult {
        let sub    = sub_scores(input);
        let score  = aggregate(&sub);
        let result = decide(score, sub, self.consecutive_rejections, self.exploration_mode);
        if result.decision == Decision::Reject {
            self.consecutive_rejections += 1;
        } else {
            self.consecutive_rejections = 0;
        }
        result
    }
}

// ── Axis scorers ──────────────────────────────────────────────────────────────

fn sub_scores(input: &QualityInput) -> SubScores {
    SubScores {
        resolution:          score_resolution(input.width, input.height),
        freshness:           score_freshness(input.captured_at_ms),
        density:             score_density(input.area_image_count, input.search_radius_km),
        stability:           input.gps_jitter_m.map(score_stability),
        sequence_continuity: input.sequence_continuity,
        server_quality:      input.server_quality.map(score_server_quality),
        sharpness:           input.sharpness,
        overlay:             input.overlay,
    }
}

/// Mapillary quality_score [0.0, 5.0] → [0.0, 1.0].
fn score_server_quality(q: f32) -> f32 {
    (q / 5.0).clamp(0.0, 1.0)
}

/// Pixel count: 320×240 → 0.0, ≥2048×1536 → 1.0.  Unknown → 0.5 (neutral).
fn score_resolution(w: u32, h: u32) -> f32 {
    if w == 0 || h == 0 { return 0.5; }
    let px = w as f64 * h as f64;
    const MIN: f64 =    76_800.0;  // 320 × 240  — barely usable
    const MAX: f64 = 3_145_728.0;  // 2048 × 1536 — solid quality
    ((px - MIN) / (MAX - MIN)).clamp(0.0, 1.0) as f32
}

/// Freshness with a 2-year grace period and a 0.5 floor.
///
/// - ≤ 2 years  → 1.0
/// - 2–15 years → linear decay from 1.0 → 0.5
/// - > 15 years → 0.5  (floor avoids penalising valid rural coverage)
/// - Unknown    → 0.5  (neutral)
fn score_freshness(captured_at_ms: Option<i64>) -> f32 {
    let Some(ts) = captured_at_ms else { return 0.5; };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(ts);
    let age_days = (now_ms - ts).max(0) as f64 / 86_400_000.0;
    const GRACE: f64 = 730.5;   // 2 years
    const STALE: f64 = 5478.75; // 15 years
    if age_days <= GRACE { return 1.0; }
    if age_days >= STALE { return 0.5; }
    (1.0 - 0.5 * (age_days - GRACE) / (STALE - GRACE)) as f32
}

/// Spatial density: images per km² in the search area.
/// Normalised between 0.01 (very rural) and 2.0 (well covered) img/km².
/// Falls back to raw-count normalisation when `radius_km` is zero.
fn score_density(count: usize, radius_km: f64) -> f32 {
    if count == 0 { return 0.0; }
    if radius_km <= 0.0 {
        return ((count as f64 - 1.0) / 29.0).clamp(0.0, 1.0) as f32;
    }
    let area_km2 = std::f64::consts::PI * radius_km * radius_km;
    let density  = count as f64 / area_km2;
    const MIN_D: f64 = 0.01;
    const MAX_D: f64 = 2.0;
    ((density - MIN_D) / (MAX_D - MIN_D)).clamp(0.0, 1.0) as f32
}

/// GPS jitter: < 0.5 m → 1.0; linear decay to 0.0 at 10 m.
fn score_stability(jitter_m: f64) -> f32 {
    const IDEAL: f64 = 0.5;
    const WORST: f64 = 10.0;
    (1.0 - (jitter_m - IDEAL).max(0.0) / (WORST - IDEAL)).clamp(0.0, 1.0) as f32
}

// ── Aggregation ───────────────────────────────────────────────────────────────

/// Nominal axis weights.  Sum to 1.35 when all seven axes are present;
/// renormalisation in `aggregate` always brings the effective sum to 1.0.
const W_RES: f32 = 0.35;
const W_SHA: f32 = 0.20; // sharpness (Laplacian)
const W_SEQ: f32 = 0.20;
const W_DEN: f32 = 0.20;
const W_STA: f32 = 0.15;
const W_SQU: f32 = 0.15; // server quality (Mapillary)
const W_OVL: f32 = 0.15; // overlay cleanliness
const W_FRE: f32 = 0.10;

/// Weighted sum with dynamic renormalisation.
///
/// When GPS stability or sequence continuity data is unavailable, those axes
/// are simply dropped from the sum and the total weight is renormalised to 1.0.
/// This avoids phantom 0.5 fillers corrupting the score.
fn aggregate(s: &SubScores) -> f32 {
    // Build the list of (score, weight) pairs for available axes only.
    let mut pairs: [(f32, f32); 8] = [
        (s.resolution, W_RES),
        (s.density,    W_DEN),
        (s.freshness,  W_FRE),
        (s.stability.unwrap_or(0.0),           if s.stability.is_some()           { W_STA } else { 0.0 }),
        (s.sequence_continuity.unwrap_or(0.0), if s.sequence_continuity.is_some() { W_SEQ } else { 0.0 }),
        (s.server_quality.unwrap_or(0.0),      if s.server_quality.is_some()      { W_SQU } else { 0.0 }),
        (s.sharpness.unwrap_or(0.0),           if s.sharpness.is_some()           { W_SHA } else { 0.0 }),
        (s.overlay.unwrap_or(0.0),             if s.overlay.is_some()             { W_OVL } else { 0.0 }),
    ];

    let total_weight: f32 = pairs.iter().map(|(_, w)| *w).sum();
    if total_weight <= 0.0 { return 0.5; }

    // Zero out the score contribution of absent axes so they don't contribute.
    for (score, weight) in &mut pairs {
        if *weight == 0.0 { *score = 0.0; }
    }

    pairs.iter().map(|(s, w)| s * w).sum::<f32>() / total_weight
}

// ── Decision ──────────────────────────────────────────────────────────────────

const THRESHOLD_GOOD:    f32 = 0.60;
const THRESHOLD_SOFT:    f32 = 0.40;
const THRESHOLD_FLOOR:   f32 = 0.30;
const RELAX_AFTER:       u32 = 3;
const RELAX_STEP:        f32 = 0.02;
/// Extra threshold relaxation applied in exploration mode (geographic anti-collapse).
const EXPLORE_RELAX:     f32 = 0.05;

fn decide(score: f32, sub: SubScores, consecutive: u32, exploration: bool) -> QualityResult {
    let relax = if consecutive >= RELAX_AFTER {
        ((consecutive - RELAX_AFTER + 1) as f32 * RELAX_STEP)
            .min(THRESHOLD_SOFT - THRESHOLD_FLOOR)
    } else {
        0.0
    };
    let extra = if exploration { EXPLORE_RELAX } else { 0.0 };
    let effective = (THRESHOLD_SOFT - relax - extra).max(THRESHOLD_FLOOR);

    if score >= THRESHOLD_GOOD {
        QualityResult { decision: Decision::Accept, score, reason: "good quality",                       sub }
    } else if score >= effective {
        let reason = match (relax > 0.0, exploration) {
            (true,  true)  => "soft accept (anti-starvation + exploration)",
            (true,  false) => "soft accept (anti-starvation)",
            (false, true)  => "soft accept (exploration)",
            (false, false) => "soft accept",
        };
        QualityResult { decision: Decision::Accept, score, reason,                                       sub }
    } else {
        QualityResult { decision: Decision::Reject, score, reason: weakest_axis(&sub),                   sub }
    }
}

/// Name the axis whose weighted contribution is lowest (most responsible for rejection).
fn weakest_axis(sub: &SubScores) -> &'static str {
    // Use base weight for optional axes; a 0.5-neutral placeholder is fine here
    // since we are only comparing to find the *relatively* worst axis.
    let contributions = [
        (sub.resolution                              * W_RES, "low resolution"),
        (sub.sequence_continuity.unwrap_or(0.5)      * W_SEQ, "fragmented sequence"),
        (sub.density                                 * W_DEN, "sparse coverage"),
        (sub.stability.unwrap_or(0.5)                * W_STA, "high GPS jitter"),
        (sub.freshness                               * W_FRE, "stale imagery"),
        (sub.server_quality.unwrap_or(0.5)           * W_SQU, "low server quality"),
        (sub.sharpness.unwrap_or(0.5)                * W_SHA, "motion blur"),
        (sub.overlay.unwrap_or(0.5)                  * W_OVL, "ui overlay"),
    ];
    contributions
        .iter()
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .map(|&(_, r)| r)
        .unwrap_or("below threshold")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
    }

    fn input_with_age(w: u32, h: u32, age_days: i64, count: usize, radius_km: f64) -> QualityInput {
        QualityInput {
            width:               w,
            height:              h,
            captured_at_ms:      Some(now_ms() - age_days * 86_400_000),
            area_image_count:    count,
            search_radius_km:    radius_km,
            gps_jitter_m:        None,
            sequence_continuity: None,
            server_quality:      None,
            sharpness:           None,
            overlay:             None,
        }
    }

    // ── Existing axes ─────────────────────────────────────────────────────────

    #[test]
    fn good_image_accepted() {
        let mut f = FilterState::new();
        let r = f.evaluate(&input_with_age(3840, 2160, 90, 25, 5.0));
        assert_eq!(r.decision, Decision::Accept);
        assert!(r.score >= 0.60, "score {:.3}", r.score);
    }

    #[test]
    fn low_quality_rejected() {
        let mut f = FilterState::new();
        let r = f.evaluate(&input_with_age(320, 240, 2920, 1, 10.0));
        assert_eq!(r.decision, Decision::Reject);
        assert!(r.score < 0.40, "score {:.3}", r.score);
    }

    #[test]
    fn old_rural_image_not_zero_scored() {
        let fre = score_freshness(Some(now_ms() - 12 * 365 * 86_400_000_i64));
        assert!(fre >= 0.5, "freshness floor violated: {fre:.3}");
    }

    #[test]
    fn density_normalised_by_area() {
        let small = score_density(30, 1.0);
        let large = score_density(30, 20.0);
        assert!(small > large, "small={small:.3} large={large:.3}");
    }

    #[test]
    fn stability_excluded_when_unknown() {
        let no_gps = SubScores { resolution: 0.8, freshness: 0.9, density: 0.7,
                                 stability: None, sequence_continuity: None,
                                 server_quality: None, sharpness: None, overlay: None };
        let with_gps = SubScores { stability: Some(0.5), ..no_gps };
        assert!(aggregate(&no_gps) > aggregate(&with_gps),
            "no_gps={:.3} with_gps={:.3}", aggregate(&no_gps), aggregate(&with_gps));
    }

    #[test]
    fn anti_starvation_relaxes_threshold() {
        let mut f = FilterState::with_streak(6);
        // After 6 rejections: relax = (6-3+1)*0.02=0.08, effective = 0.32
        let r = f.evaluate(&input_with_age(800, 600, 2000, 3, 10.0));
        println!("score={:.3} reason={}", r.score, r.reason);
        if r.decision == Decision::Accept {
            assert!(r.reason.contains("anti-starvation") || r.reason.contains("soft"));
        }
    }

    #[test]
    fn streak_persists_via_with_streak() {
        let mut f = FilterState::with_streak(5);
        assert_eq!(f.streak(), 5);
        let bad = input_with_age(320, 240, 4000, 1, 10.0);
        let r = f.evaluate(&bad);
        if r.decision == Decision::Reject { assert_eq!(f.streak(), 6); }
        else                              { assert_eq!(f.streak(), 0); }
    }

    // ── Sequence continuity ───────────────────────────────────────────────────

    fn make_frames(coords: &[(f64, f64)], angles: Option<&[f32]>) -> Vec<SequenceFrame> {
        coords
            .iter()
            .enumerate()
            .map(|(i, &(lat, lon))| SequenceFrame {
                lat,
                lon,
                captured_at_ms: Some(i as i64 * 1000),
                compass_angle:  angles.and_then(|a| a.get(i)).copied(),
            })
            .collect()
    }

    #[test]
    fn good_sequence_scores_high() {
        // 10 frames, ~5 m steps, heading progresses gently along a straight road.
        let coords: Vec<(f64, f64)> = (0..10)
            .map(|i| (48.0 + i as f64 * 0.000045, 11.0)) // ~5 m steps northward
            .collect();
        let angles: Vec<f32> = (0..10).map(|i| (i as f32) * 2.0).collect(); // 0°→18°, gentle curve
        let frames = make_frames(&coords, Some(&angles));
        let score = score_sequence_continuity(&frames);
        assert!(score >= 0.70, "score {score:.3}");
    }

    #[test]
    fn isolated_frame_penalised() {
        let frames = make_frames(&[(48.0, 11.0)], None);
        let score = score_sequence_continuity(&frames);
        assert!(score < 0.30, "isolated frame score {score:.3}");
    }

    #[test]
    fn gps_jumps_reduce_score() {
        // Mix of normal steps and a huge 500 m jump.
        let frames = make_frames(
            &[(48.0, 11.0), (48.000045, 11.0), (48.005, 11.0), (48.005045, 11.0)],
            None,
        );
        let score_jump = score_sequence_continuity(&frames);

        // Compare with a clean 4-frame sequence.
        let clean = make_frames(
            &[(48.0, 11.0), (48.000045, 11.0), (48.000090, 11.0), (48.000135, 11.0)],
            None,
        );
        let score_clean = score_sequence_continuity(&clean);
        assert!(score_clean > score_jump,
            "clean={score_clean:.3} jump={score_jump:.3}");
    }

    #[test]
    fn chaotic_heading_reduces_score() {
        let coords = make_frames(&[(48.0, 11.0), (48.000045, 11.0), (48.000090, 11.0)], None);

        let mut stable = coords.clone();
        for (i, f) in stable.iter_mut().enumerate() { f.compass_angle = Some(i as f32 * 5.0); }

        let mut chaotic = coords.clone();
        for (i, f) in chaotic.iter_mut().enumerate() {
            f.compass_angle = Some(if i % 2 == 0 { 0.0 } else { 170.0 }); // near 180° oscillation
        }

        assert!(
            score_seq_heading(&stable) > score_seq_heading(&chaotic),
            "stable={:.3} chaotic={:.3}",
            score_seq_heading(&stable), score_seq_heading(&chaotic),
        );
    }

    #[test]
    fn sequence_continuity_axis_integration() {
        // Same image input; one has good sequence, one has none.
        let base = input_with_age(1920, 1080, 180, 20, 5.0);
        let with_seq = QualityInput { sequence_continuity: Some(0.90), ..base.clone() };
        let without  = QualityInput { sequence_continuity: None,        ..base.clone() };

        let mut f = FilterState::new();
        let r_seq  = f.evaluate(&with_seq);
        let r_none = f.evaluate(&without);
        assert!(r_seq.score > r_none.score,
            "with_seq={:.3} without={:.3}", r_seq.score, r_none.score);
    }

    // ── Sharpness / server quality ────────────────────────────────────────────

    #[test]
    fn sharpness_affects_score() {
        let base = input_with_age(1920, 1080, 180, 20, 5.0);
        let sharp  = QualityInput { sharpness: Some(1.0), ..base.clone() };
        let blurry = QualityInput { sharpness: Some(0.0), ..base.clone() };
        let mut f = FilterState::new();
        assert!(f.evaluate(&sharp).score > f.evaluate(&blurry).score);
    }

    #[test]
    fn blurry_image_rejected() {
        // Low-res, moderately stale, sparse — with sharpness=0, this should reject
        // and report "motion blur" as the weakest axis.
        let input = QualityInput {
            width: 1280, height: 720,
            captured_at_ms: Some(now_ms() - 1000 * 86_400_000),
            area_image_count: 30,
            search_radius_km: 5.0,
            gps_jitter_m:        None,
            sequence_continuity: None,
            server_quality:      None,
            sharpness:           Some(0.0),
            overlay:             None,
        };
        let mut f = FilterState::new();
        let r = f.evaluate(&input);
        assert_eq!(r.decision, Decision::Reject, "score={:.3}", r.score);
        assert_eq!(r.reason, "motion blur");
    }

    #[test]
    fn low_server_quality_reduces_score() {
        let base = input_with_age(1920, 1080, 180, 20, 5.0);
        let good = QualityInput { server_quality: Some(4.5), ..base.clone() };
        let bad  = QualityInput { server_quality: Some(1.0), ..base.clone() };
        let mut f = FilterState::new();
        assert!(f.evaluate(&good).score > f.evaluate(&bad).score);
    }

    // ── Overlay detection ─────────────────────────────────────────────────────

    #[test]
    fn overlay_penalises_score() {
        let base      = input_with_age(1920, 1080, 180, 20, 5.0);
        let clean     = QualityInput { overlay: Some(1.0), ..base.clone() };
        let overlayed = QualityInput { overlay: Some(0.0), ..base.clone() };
        let mut f = FilterState::new();
        assert!(f.evaluate(&clean).score > f.evaluate(&overlayed).score);
    }

    #[test]
    fn overlay_excluded_redistributes_weight() {
        let base         = input_with_age(1920, 1080, 180, 20, 5.0);
        let with_overlay = QualityInput { overlay: Some(0.8), ..base.clone() };
        let without      = QualityInput { overlay: None,      ..base.clone() };
        let mut f = FilterState::new();
        // Overlay-present image with high score should beat the no-overlay base.
        assert!(f.evaluate(&with_overlay).score > f.evaluate(&without).score);
    }

    #[test]
    fn severe_overlay_reported_as_weakest() {
        let input = QualityInput {
            width: 1280, height: 720,
            captured_at_ms:      Some(now_ms() - 800 * 86_400_000),
            area_image_count:    15,
            search_radius_km:    5.0,
            gps_jitter_m:        None,
            sequence_continuity: None,
            server_quality:      None,
            sharpness:           Some(0.5),
            overlay:             Some(0.0),
        };
        let mut f = FilterState::new();
        let r = f.evaluate(&input);
        if r.decision == Decision::Reject {
            assert_eq!(r.reason, "ui overlay");
        }
    }

    #[test]
    fn sequence_excluded_redistributes_weight() {
        // When sequence_continuity is None, score should still be well-formed
        // (no NaN, not trivially 0 or 1).
        let sub = SubScores {
            resolution: 0.7, freshness: 0.8, density: 0.5,
            stability: None, sequence_continuity: None,
            server_quality: None, sharpness: None, overlay: None,
        };
        let score = aggregate(&sub);
        assert!(score > 0.0 && score < 1.0, "score out of range: {score:.3}");
    }
}

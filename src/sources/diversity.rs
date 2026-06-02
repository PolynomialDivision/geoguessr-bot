//! Geographic diversity tracking for the prefetch pipeline.
//!
//! Divides the Earth into 2° × 2° lat/lon grid cells (~220 km at the equator).
//! Counts how many accepted locations fall into each cell so the fetch pipeline
//! can prefer under-sampled regions and detect geographic collapse.

use std::collections::HashMap;

/// Grid cell resolution in degrees.
const CELL_DEG: f64 = 2.0;

fn cell_key(lat: f64, lon: f64) -> (i32, i32) {
    ((lat / CELL_DEG).floor() as i32, (lon / CELL_DEG).floor() as i32)
}

/// Lightweight grid-based geographic diversity tracker.
///
/// Scores new candidates in (0.0, 1.0]: 1.0 for an empty cell, 1/(count+1)
/// for occupied cells.  Never reaches 0.0 so heavily-sampled areas can still
/// be selected under anti-starvation pressure.
#[derive(Debug, Default, Clone)]
pub struct DiversityTracker {
    cell_counts: HashMap<(i32, i32), u32>,
}

impl DiversityTracker {
    pub fn new() -> Self { Self::default() }

    /// Build from an existing slice of (lat, lon) pairs.
    pub fn from_coords(coords: &[(f64, f64)]) -> Self {
        let mut t = Self::new();
        for &(lat, lon) in coords { t.accept(lat, lon); }
        t
    }

    /// Register a newly accepted location.
    pub fn accept(&mut self, lat: f64, lon: f64) {
        *self.cell_counts.entry(cell_key(lat, lon)).or_insert(0) += 1;
    }

    /// Diversity score for a candidate at (lat, lon).
    ///
    /// Returns 1.0 for an empty cell and 1/(count+1) for occupied cells.
    pub fn score(&self, lat: f64, lon: f64) -> f32 {
        let count = self.cell_counts.get(&cell_key(lat, lon)).copied().unwrap_or(0);
        1.0 / (count as f32 + 1.0)
    }

    /// Returns `true` when the top-3 most-sampled cells hold more than half of
    /// all recorded locations — the cache has collapsed into a small cluster.
    pub fn is_homogeneous(&self) -> bool {
        let total: u32 = self.cell_counts.values().sum();
        if total < 6 { return false; }
        let mut counts: Vec<u32> = self.cell_counts.values().copied().collect();
        counts.sort_unstable_by(|a, b| b.cmp(a));
        let top3: u32 = counts.iter().take(3).sum();
        top3 as f32 / total as f32 > 0.5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cell_scores_one() {
        let t = DiversityTracker::new();
        assert!((t.score(48.0, 11.0) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn occupied_cell_scores_lower() {
        let mut t = DiversityTracker::new();
        t.accept(48.0, 11.0);
        let s = t.score(48.0, 11.0);
        assert!(s < 1.0 && s > 0.0, "score={s}");
    }

    #[test]
    fn different_cells_independent() {
        let mut t = DiversityTracker::new();
        t.accept(48.0, 11.0);
        assert!((t.score(0.0, 0.0) - 1.0).abs() < f32::EPSILON, "unrelated cell should score 1.0");
    }

    #[test]
    fn score_decreases_with_saturation() {
        let mut t = DiversityTracker::new();
        let s0 = t.score(48.0, 11.0);
        t.accept(48.0, 11.0);
        let s1 = t.score(48.0, 11.0);
        t.accept(48.0, 11.0);
        let s2 = t.score(48.0, 11.0);
        assert!(s0 > s1 && s1 > s2, "s0={s0} s1={s1} s2={s2}");
    }

    #[test]
    fn homogeneous_when_clustered() {
        let mut t = DiversityTracker::new();
        for _ in 0..8 { t.accept(48.0, 11.0); }
        t.accept(0.0, 0.0);
        assert!(t.is_homogeneous());
    }

    #[test]
    fn not_homogeneous_when_spread() {
        let mut t = DiversityTracker::new();
        for i in 0..10 { t.accept(i as f64 * 10.0, 0.0); }
        assert!(!t.is_homogeneous());
    }

    #[test]
    fn not_homogeneous_with_few_samples() {
        let mut t = DiversityTracker::new();
        for _ in 0..5 { t.accept(48.0, 11.0); }
        // below the 6-sample minimum — should not trigger
        assert!(!t.is_homogeneous());
    }
}

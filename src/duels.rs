//! Duel scoring: on every image each player meets every other player once,
//! and the one closer to the answer wins that duel.
//!
//! - Closer guess (to the metre) wins +1, the same metre is a tie, ½ each.
//! - A player who joined the round but did not guess loses every duel
//!   against those who did, so skipping a hard image never helps.
//! - Two players who both missed have no duel.
//! - A lone guess has nobody to beat: it counts as played, not as a duel.
//!
//! Points are kept in halves (`i64`) so ties stay exact. Everything here is
//! derived from stored distances only, so a result never changes after the
//! image closes and any period can be recomputed from history.

use std::collections::HashMap;

/// One player's row on one image: `dist_km` is `None` for a missed guess.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub image: i64,
    pub user_id: String,
    pub dist_km: Option<f64>,
}

/// One player's result on one image.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageResult {
    pub user_id: String,
    /// Duel points won, in halves.
    pub halves: i64,
    /// Duels played on this image.
    pub duels: i64,
    /// 1-based place among real guesses (shared on the same metre), `None`
    /// for a missed guess.
    pub place: Option<usize>,
}

/// Whole metres, the precision every comparison uses.
pub fn metres(dist_km: f64) -> i64 {
    (dist_km * 1000.0).round() as i64
}

/// Duel results for one image, best first (missed guesses last).
pub fn score_image(entries: &[Entry]) -> Vec<ImageResult> {
    let mut real: Vec<(&str, i64)> = entries
        .iter()
        .filter_map(|e| e.dist_km.map(|d| (e.user_id.as_str(), metres(d))))
        .collect();
    real.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(b.0)));
    let missed: Vec<&str> = {
        let mut m: Vec<&str> = entries
            .iter()
            .filter(|e| e.dist_km.is_none())
            .map(|e| e.user_id.as_str())
            .collect();
        m.sort();
        m
    };
    let n = real.len() as i64;

    let mut out = Vec::with_capacity(entries.len());
    for &(uid, m) in &real {
        let closer = real.iter().filter(|o| o.1 < m).count();
        let tied = real.iter().filter(|o| o.1 == m).count() as i64 - 1;
        let beaten = n - 1 - closer as i64 - tied;
        out.push(ImageResult {
            user_id: uid.to_owned(),
            halves: 2 * (beaten + missed.len() as i64) + tied,
            duels: n - 1 + missed.len() as i64,
            place: Some(closer + 1),
        });
    }
    for uid in missed {
        out.push(ImageResult {
            user_id: uid.to_owned(),
            halves: 0,
            duels: n,
            place: None,
        });
    }
    out
}

/// A player's totals over a period.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Standing {
    pub user_id: String,
    /// Duel points won, in halves.
    pub halves: i64,
    pub duels: i64,
    /// Images won outright or shared, against at least one other guess.
    pub wins: i64,
    /// Images this player was in (guessed or missed).
    pub images: i64,
    pub missed: i64,
    /// Closest guess in the period.
    pub best_km: Option<f64>,
}

impl Standing {
    /// Share of duels won, 0.0 to 1.0, `None` without any duel.
    pub fn rate(&self) -> Option<f64> {
        (self.duels > 0).then(|| self.halves as f64 / (2 * self.duels) as f64)
    }
}

/// Totals per player over all `entries` (any number of images).
pub fn standings(entries: &[Entry]) -> Vec<Standing> {
    let mut by_image: HashMap<i64, Vec<Entry>> = HashMap::new();
    for e in entries {
        by_image.entry(e.image).or_default().push(e.clone());
    }
    let mut totals: HashMap<String, Standing> = HashMap::new();
    for image in by_image.values() {
        let real = image.iter().filter(|e| e.dist_km.is_some()).count();
        for r in score_image(image) {
            let s = totals.entry(r.user_id.clone()).or_insert_with(|| Standing {
                user_id: r.user_id.clone(),
                ..Standing::default()
            });
            s.halves += r.halves;
            s.duels += r.duels;
            s.images += 1;
            match r.place {
                Some(1) if real >= 2 => s.wins += 1,
                None => s.missed += 1,
                _ => {}
            }
        }
        for e in image {
            if let Some(d) = e.dist_km {
                let s = totals.get_mut(&e.user_id).expect("scored above");
                s.best_km = Some(s.best_km.map_or(d, |b| b.min(d)));
            }
        }
    }
    totals.into_values().collect()
}

fn best_cmp(a: &Standing, b: &Standing) -> std::cmp::Ordering {
    let inf = f64::INFINITY;
    a.best_km
        .unwrap_or(inf)
        .partial_cmp(&b.best_km.unwrap_or(inf))
        .unwrap_or(std::cmp::Ordering::Equal)
}

fn rate_cmp(a: &Standing, b: &Standing) -> std::cmp::Ordering {
    b.rate()
        .unwrap_or(0.0)
        .partial_cmp(&a.rate().unwrap_or(0.0))
        .unwrap_or(std::cmp::Ordering::Equal)
}

/// Season order (the month): most duel points, then the better share, more
/// images won, the closest guess.
pub fn sort_by_points(board: &mut [Standing]) {
    board.sort_by(|a, b| {
        b.halves
            .cmp(&a.halves)
            .then_with(|| rate_cmp(a, b))
            .then(b.wins.cmp(&a.wins))
            .then_with(|| best_cmp(a, b))
            .then(a.user_id.cmp(&b.user_id))
    });
}

/// Skill order (90 days, all time): best share of duels won, then more duel
/// points, more images won, the closest guess.
pub fn sort_by_rate(board: &mut [Standing]) {
    board.sort_by(|a, b| {
        rate_cmp(a, b)
            .then(b.halves.cmp(&a.halves))
            .then(b.wins.cmp(&a.wins))
            .then_with(|| best_cmp(a, b))
            .then(a.user_id.cmp(&b.user_id))
    });
}

/// Duel points for display: "3", "3½", "½".
pub fn fmt_points(halves: i64) -> String {
    match (halves / 2, halves % 2) {
        (0, 1) => "½".to_owned(),
        (w, 0) => w.to_string(),
        (w, _) => format!("{w}½"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(image: i64, user: &str, m: Option<f64>) -> Entry {
        Entry {
            image,
            user_id: user.to_owned(),
            dist_km: m.map(|m| m / 1000.0),
        }
    }

    fn by_user(results: &[ImageResult], user: &str) -> ImageResult {
        results.iter().find(|r| r.user_id == user).unwrap().clone()
    }

    #[test]
    fn the_closer_guess_wins_by_a_single_metre() {
        let r = score_image(&[e(1, "a", Some(7.0)), e(1, "b", Some(8.0))]);
        assert_eq!(r[0].user_id, "a");
        assert_eq!((r[0].halves, r[0].duels, r[0].place), (2, 1, Some(1)));
        assert_eq!((r[1].halves, r[1].duels, r[1].place), (0, 1, Some(2)));
    }

    #[test]
    fn the_same_metre_is_a_tie_and_shares_the_place() {
        let r = score_image(&[
            e(1, "a", Some(12.2)),
            e(1, "b", Some(11.8)),
            e(1, "c", Some(900.0)),
        ]);
        let (a, b, c) = (by_user(&r, "a"), by_user(&r, "b"), by_user(&r, "c"));
        assert_eq!((a.halves, a.place), (3, Some(1)), "beat c, tied b");
        assert_eq!((b.halves, b.place), (3, Some(1)));
        assert_eq!((c.halves, c.place), (0, Some(3)));
    }

    #[test]
    fn beating_three_players_is_worth_three_and_second_of_two_is_worth_nothing() {
        let four = score_image(&[
            e(1, "a", Some(5.0)),
            e(1, "b", Some(50.0)),
            e(1, "c", Some(500.0)),
            e(1, "d", Some(5000.0)),
        ]);
        let pts: Vec<i64> = four.iter().map(|r| r.halves / 2).collect();
        assert_eq!(pts, vec![3, 2, 1, 0]);
        let two = score_image(&[e(1, "a", Some(5.0)), e(1, "b", Some(6.0))]);
        assert_eq!(by_user(&two, "b").halves, 0);
    }

    #[test]
    fn a_missed_guess_loses_to_everyone_who_guessed() {
        let r = score_image(&[e(1, "a", Some(5.0)), e(1, "b", Some(9e6)), e(1, "z", None)]);
        assert_eq!(by_user(&r, "a").halves, 4);
        assert_eq!(
            by_user(&r, "b").halves,
            2,
            "even a wild guess beats skipping"
        );
        let z = by_user(&r, "z");
        assert_eq!((z.halves, z.duels, z.place), (0, 2, None));
        assert_eq!(r.last().unwrap().user_id, "z");
    }

    #[test]
    fn two_missed_players_have_no_duel_and_a_lone_guess_has_none_either() {
        let r = score_image(&[e(1, "y", None), e(1, "z", None)]);
        assert!(r.iter().all(|r| r.duels == 0));
        let solo = score_image(&[e(1, "a", Some(3.0))]);
        assert_eq!(
            (solo[0].halves, solo[0].duels, solo[0].place),
            (0, 0, Some(1))
        );
    }

    #[test]
    fn standings_add_up_points_wins_misses_and_the_best_guess() {
        let rows = vec![
            e(1, "a", Some(3.0)),
            e(1, "b", Some(40.0)),
            e(2, "a", None),
            e(2, "b", Some(700.0)),
            e(3, "a", Some(1.0)),
        ];
        let board = standings(&rows);
        let a = board.iter().find(|s| s.user_id == "a").unwrap();
        let b = board.iter().find(|s| s.user_id == "b").unwrap();
        assert_eq!(
            (a.halves, a.duels, a.wins, a.images, a.missed),
            (2, 2, 1, 3, 1)
        );
        // Beating only someone who skipped wins the duel, not the image.
        assert_eq!(
            (b.halves, b.duels, b.wins, b.images, b.missed),
            (2, 2, 0, 2, 0)
        );
        assert_eq!(a.best_km, Some(0.001));
        assert_eq!(a.rate(), Some(0.5));
    }

    #[test]
    fn a_lone_guess_is_not_an_image_win() {
        let board = standings(&[e(1, "a", Some(1.0))]);
        assert_eq!(
            (board[0].wins, board[0].images, board[0].rate()),
            (0, 1, None)
        );
    }

    #[test]
    fn the_month_counts_points_and_the_long_run_counts_the_share() {
        // `busy` beats `rare` on points by playing more, `rare` wins every duel.
        let mut rows = Vec::new();
        for i in 0..6 {
            rows.push(e(i, "busy", Some(10.0)));
            rows.push(e(i, "x", Some(20.0)));
            rows.push(e(i, "y", Some(30.0)));
        }
        for i in 6..8 {
            rows.push(e(i, "rare", Some(1.0)));
            rows.push(e(i, "busy", Some(10.0)));
        }
        let mut board = standings(&rows);
        sort_by_points(&mut board);
        assert_eq!(board[0].user_id, "busy");
        sort_by_rate(&mut board);
        assert_eq!(board[0].user_id, "rare");
    }

    #[test]
    fn equal_points_and_share_fall_back_to_more_wins_then_the_closest_guess() {
        let mut board = vec![
            Standing {
                user_id: "@a".into(),
                halves: 4,
                duels: 4,
                wins: 1,
                best_km: Some(0.002),
                ..Standing::default()
            },
            Standing {
                user_id: "@b".into(),
                halves: 4,
                duels: 4,
                wins: 1,
                best_km: Some(0.001),
                ..Standing::default()
            },
        ];
        sort_by_points(&mut board);
        assert_eq!(board[0].user_id, "@b");
    }

    #[test]
    fn half_points_read_naturally() {
        assert_eq!(fmt_points(0), "0");
        assert_eq!(fmt_points(1), "½");
        assert_eq!(fmt_points(6), "3");
        assert_eq!(fmt_points(7), "3½");
    }
}

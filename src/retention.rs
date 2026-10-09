//! Retention selector shared by box GC and remote sweep.
//!
//! Policy (ADR-0008, ADR-0010): retain a generation if it has fewer
//! than N newer generations in its (flake, branch, attr) group, OR it is
//! younger than M. Count retention lapses once the group is stale: its newest
//! generation trails the newest of its flake's tier (main or other) by S.
//! Box marking uses counts only (M = S = zero).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

#[derive(Debug, Clone, Copy)]
pub struct GroupPolicy {
    pub keep_newest: usize,
    pub max_age: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub main: GroupPolicy,
    pub other: GroupPolicy,
    /// Zero disables. Measured against the flake's own tier, not the clock,
    /// so a flake that stops publishing keeps its newest generations.
    pub stale_after: Duration,
}

pub const WEEK: Duration = Duration::from_secs(7 * 24 * 3600);

impl Policy {
    /// Remote sweep defaults: main N=5 M=4wk, non-main N=1 M=1wk.
    pub fn remote() -> Self {
        Policy {
            main: GroupPolicy {
                keep_newest: 5,
                max_age: 4 * WEEK,
            },
            other: GroupPolicy {
                keep_newest: 1,
                max_age: WEEK,
            },
            stale_after: 4 * WEEK,
        }
    }

    /// Box marking defaults: newest 3 (main) / 1 (non-main), count only.
    /// Guarantees box retained ⊆ remote retained.
    pub fn boxed() -> Self {
        Policy {
            main: GroupPolicy {
                keep_newest: 3,
                max_age: Duration::ZERO,
            },
            other: GroupPolicy {
                keep_newest: 1,
                max_age: Duration::ZERO,
            },
            stale_after: Duration::ZERO,
        }
    }
}

pub struct Gen {
    /// Opaque handle returned for retained gens (e.g. object key or path).
    pub id: String,
    pub flake: String,
    pub branch: String,
    pub attr: String,
    pub time: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    KeepAge,
    KeepCount,
    /// Among the newest N, but the group is stale.
    DropStale,
    Drop,
}

impl Verdict {
    pub fn keeps(self) -> bool {
        matches!(self, Verdict::KeepAge | Verdict::KeepCount)
    }
}

/// One verdict per generation, in `gens` order.
pub fn verdicts(gens: &[Gen], policy: &Policy, now: SystemTime) -> Vec<Verdict> {
    let mut groups: HashMap<(&str, &str, &str), Vec<usize>> = HashMap::new();
    let mut tier_newest: HashMap<(&str, bool), SystemTime> = HashMap::new();
    for (i, g) in gens.iter().enumerate() {
        groups
            .entry((g.flake.as_str(), g.branch.as_str(), g.attr.as_str()))
            .or_default()
            .push(i);
        let t = tier_newest
            .entry((g.flake.as_str(), g.branch == "main"))
            .or_insert(g.time);
        *t = (*t).max(g.time);
    }
    let mut out = vec![Verdict::Drop; gens.len()];
    for ((flake, branch, _), mut group) in groups {
        let p = if branch == "main" {
            policy.main
        } else {
            policy.other
        };
        group.sort_by_key(|&i| std::cmp::Reverse(gens[i].time));
        let newest = gens[group[0]].time;
        let lead = tier_newest[&(flake, branch == "main")]
            .duration_since(newest)
            .unwrap_or(Duration::ZERO);
        let stale = !policy.stale_after.is_zero() && lead >= policy.stale_after;
        for (idx, &i) in group.iter().enumerate() {
            let age = now.duration_since(gens[i].time).unwrap_or(Duration::ZERO);
            out[i] = if age < p.max_age {
                Verdict::KeepAge
            } else if idx >= p.keep_newest {
                Verdict::Drop
            } else if stale {
                Verdict::DropStale
            } else {
                Verdict::KeepCount
            };
        }
    }
    out
}

/// Ids of the generations to retain.
pub fn retain(gens: &[Gen], policy: &Policy, now: SystemTime) -> HashSet<String> {
    gens.iter()
        .zip(verdicts(gens, policy, now))
        .filter(|(_, v)| v.keeps())
        .map(|(g, _)| g.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(id: &str, branch: &str, attr: &str, days_ago: u64, now: SystemTime) -> Gen {
        Gen {
            id: id.into(),
            flake: "znix".into(),
            branch: branch.into(),
            attr: attr.into(),
            time: now - Duration::from_secs(days_ago * 24 * 3600),
        }
    }

    #[test]
    fn remote_keeps_newest_n_and_young() {
        let now = SystemTime::now();
        // 8 main gens, one per week: idx 0..4 kept by N=5; idx 0..3 also young (<4wk).
        let gens: Vec<Gen> = (0..8)
            .map(|i| mk(&format!("g{i}"), "main", "a", i * 7, now))
            .collect();
        let keep = retain(&gens, &Policy::remote(), now);
        assert_eq!(
            keep,
            ["g0", "g1", "g2", "g3", "g4"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        );
    }

    #[test]
    fn young_gen_kept_beyond_n() {
        let now = SystemTime::now();
        // 7 main gens all pushed today: N=5 alone would drop two, age<4wk keeps all.
        let gens: Vec<Gen> = (0..7)
            .map(|i| mk(&format!("g{i}"), "main", "a", 0, now))
            .collect();
        assert_eq!(retain(&gens, &Policy::remote(), now).len(), 7);
    }

    #[test]
    fn nonmain_keeps_one_plus_young() {
        let now = SystemTime::now();
        let gens = vec![
            mk("new", "feat", "a", 0, now),
            mk("mid", "feat", "a", 3, now),  // < 1wk: kept by age
            mk("old", "feat", "a", 30, now), // dropped
        ];
        let keep = retain(&gens, &Policy::remote(), now);
        assert_eq!(keep, ["new", "mid"].iter().map(|s| s.to_string()).collect());
    }

    #[test]
    fn groups_are_independent() {
        let now = SystemTime::now();
        let gens = vec![
            mk("m1", "main", "a", 100, now),
            mk("m2", "main", "b", 100, now),
            mk("f1", "feat", "a", 100, now),
        ];
        // Old, but each is the newest of its own (branch, attr) group.
        let keep = retain(&gens, &Policy::remote(), now);
        assert_eq!(keep.len(), 3);
    }

    #[test]
    fn stale_groups_lose_count_retention() {
        let now = SystemTime::now();
        let gens = vec![
            mk("live", "main", "a", 0, now),
            mk("renamed", "main", "gone", 40, now), // trails main by 40d
            mk("pr", "pr", "a", 0, now),
            mk("tag", "v0.1.0", "a", 40, now), // trails the other tier by 40d
        ];
        let v = verdicts(&gens, &Policy::remote(), now);
        assert_eq!(
            v,
            [
                Verdict::KeepAge,
                Verdict::DropStale,
                Verdict::KeepAge,
                Verdict::DropStale
            ]
        );
    }

    #[test]
    fn staleness_is_relative_to_the_flake_tier_not_the_clock() {
        let now = SystemTime::now();
        let mut gens = vec![
            // Dormant flake: nothing in 100 days, attrs within S of each other.
            mk("a", "main", "a", 100, now),
            mk("b", "main", "b", 110, now),
        ];
        // Other-tier churn never makes main stale.
        gens.push(mk("pr", "pr", "a", 0, now));
        let keep = retain(&gens, &Policy::remote(), now);
        assert!(keep.contains("a") && keep.contains("b"));

        let mut off = Policy::remote();
        off.stale_after = Duration::ZERO;
        let gens = vec![
            mk("live", "main", "a", 0, now),
            mk("renamed", "main", "gone", 400, now),
        ];
        assert!(retain(&gens, &off, now).contains("renamed"));
    }

    #[test]
    fn staleness_never_overrides_age() {
        let now = SystemTime::now();
        let mut p = Policy::remote();
        p.stale_after = WEEK;
        let gens = vec![
            mk("live", "main", "a", 0, now),
            mk("young", "main", "gone", 10, now), // stale, but < M=4wk
        ];
        assert!(retain(&gens, &p, now).contains("young"));
    }

    #[test]
    fn box_marking_is_count_only_and_subset_of_remote() {
        let now = SystemTime::now();
        let gens: Vec<Gen> = (0..6)
            .map(|i| mk(&format!("g{i}"), "main", "a", i, now))
            .collect();
        let boxed = retain(&gens, &Policy::boxed(), now);
        let remote = retain(&gens, &Policy::remote(), now);
        assert_eq!(boxed.len(), 3); // newest 3 only, age ignored
        assert!(boxed.is_subset(&remote));
    }
}

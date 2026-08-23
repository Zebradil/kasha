//! Retention selector shared by box GC and remote sweep.
//!
//! Policy (ADR-0008): retain a generation if it has fewer
//! than N newer generations in its (flake, branch, attr) group, OR it is
//! younger than M. Box marking uses counts only (M = zero).

use std::collections::HashSet;
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

/// Ids of the generations to retain.
pub fn retain(gens: &[Gen], policy: &Policy, now: SystemTime) -> HashSet<String> {
    let mut groups: std::collections::HashMap<(&str, &str, &str), Vec<&Gen>> =
        std::collections::HashMap::new();
    for g in gens {
        groups
            .entry((g.flake.as_str(), g.branch.as_str(), g.attr.as_str()))
            .or_default()
            .push(g);
    }
    let mut keep = HashSet::new();
    for ((_, branch, _), mut group) in groups {
        let p = if branch == "main" {
            policy.main
        } else {
            policy.other
        };
        group.sort_by_key(|g| std::cmp::Reverse(g.time));
        for (idx, g) in group.iter().enumerate() {
            let age = now.duration_since(g.time).unwrap_or(Duration::ZERO);
            if idx < p.keep_newest || age < p.max_age {
                keep.insert(g.id.clone());
            }
        }
    }
    keep
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

//! The score and the keep rule.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::trial::TrialRecord;

/// 95% Wilson score interval for `passed` of `n` trials.
pub fn wilson(passed: usize, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 0.0);
    }
    let z: f64 = 1.96;
    let n = n as f64;
    let p = passed as f64 / n;
    let denom = 1.0 + z * z / n;
    let centre = (p + z * z / (2.0 * n)) / denom;
    let half = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt() / denom;
    ((centre - half).max(0.0), (centre + half).min(1.0))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Kept,
    Tie,
    Reverted,
}

/// Exploratory development ranking: retain a nondecreasing point estimate
/// and lower bound, recording exact ties. This rule ignores cost and repeated
/// selection; it is not statistical evidence for promotion.
pub fn keep_decision(score: f64, ci_low: f64, best_score: f64, best_low: f64) -> Decision {
    if ci_low < best_low || score < best_score {
        Decision::Reverted
    } else if ci_low == best_low && score == best_score {
        Decision::Tie
    } else {
        Decision::Kept
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cost {
    /// Reported token totals; unknown if any contributing trial is unknown.
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_tokens: Option<u64>,
    pub tool_calls: u64,
    pub wall_seconds: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Summary {
    /// Mean reward over every counted trial (contaminated trials excluded).
    pub score: f64,
    /// Mean over tasks of the fraction of attempts that passed.
    pub pass1: f64,
    /// Fraction of tasks with at least one passing attempt.
    pub passk: f64,
    pub ci_low: f64,
    pub ci_high: f64,
    pub trials: usize,
    pub tasks: usize,
    /// Trials whose transcript referenced the public benchmark's own material.
    /// Excluded from score, pass rates and the interval; their cost still counts.
    #[serde(default)]
    pub contaminated: usize,
    /// Per task, the reward of each attempt in order (contaminated attempts
    /// recorded as NaN so their position is kept without scoring them).
    pub rewards: BTreeMap<String, Vec<f64>>,
    pub cost: Cost,
}

pub fn summarize(all: &[TrialRecord]) -> Summary {
    let contaminated = all.iter().filter(|t| t.contaminated).count();
    let mut rewards: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for t in all {
        rewards
            .entry(t.task.clone())
            .or_default()
            .push(if t.contaminated { f64::NAN } else { t.reward });
    }
    let trials: Vec<&TrialRecord> = all.iter().filter(|t| !t.contaminated).collect();
    let mut by_task: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for t in &trials {
        by_task.entry(t.task.clone()).or_default().push(t.reward);
    }
    let n = trials.len();
    let passed = trials.iter().filter(|t| t.reward >= 1.0).count();
    let score = if n > 0 {
        trials.iter().map(|t| t.reward).sum::<f64>() / n as f64
    } else {
        0.0
    };
    let tasks = by_task.len();
    let pass1 = if tasks > 0 {
        by_task
            .values()
            .map(|rs| rs.iter().filter(|r| **r >= 1.0).count() as f64 / rs.len() as f64)
            .sum::<f64>()
            / tasks as f64
    } else {
        0.0
    };
    let passk = if tasks > 0 {
        by_task
            .values()
            .filter(|rs| rs.iter().any(|r| *r >= 1.0))
            .count() as f64
            / tasks as f64
    } else {
        0.0
    };
    let (ci_low, ci_high) = wilson(passed, n);
    let trials = all;
    let cost = Cost {
        input_tokens: token_total(trials.iter().map(|t| t.input_tokens)),
        output_tokens: token_total(trials.iter().map(|t| t.output_tokens)),
        cache_tokens: token_total(trials.iter().map(|t| t.cache_tokens)),
        tool_calls: trials.iter().map(|t| t.tool_calls).sum(),
        wall_seconds: trials.iter().map(|t| t.wall_seconds).sum(),
    };
    Summary {
        score,
        pass1,
        passk,
        ci_low,
        ci_high,
        trials: n,
        tasks,
        contaminated,
        rewards,
        cost,
    }
}

fn token_total(mut values: impl Iterator<Item = Option<u64>>) -> Option<u64> {
    values.try_fold(0u64, |total, value| total.checked_add(value?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trials(spec: &[(&str, &[f64])]) -> Vec<TrialRecord> {
        let mut out = Vec::new();
        for (task, rewards) in spec {
            for (i, r) in rewards.iter().enumerate() {
                out.push(TrialRecord {
                    task: (*task).to_owned(),
                    attempt: i + 1,
                    contaminated: false,
                    reward: *r,
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    cache_tokens: Some(0),
                    tool_calls: 3,
                    wall_seconds: 1.0,
                    settled: true,
                    error: None,
                });
            }
        }
        out
    }

    #[test]
    fn contaminated_trials_are_neither_passes_nor_failures_but_still_cost() {
        let mut records = trials(&[("a", &[1.0, 1.0]), ("b", &[0.0])]);
        records[0].contaminated = true;
        let s = summarize(&records);
        assert_eq!(s.contaminated, 1);
        assert_eq!(s.trials, 2);
        assert_eq!(s.score, 0.5);
        assert_eq!(s.pass1, 0.5);
        assert_eq!(s.passk, 0.5);
        assert_eq!(s.ci_low, wilson(1, 2).0);
        assert!(s.rewards["a"][0].is_nan() && s.rewards["a"][1] == 1.0);
        assert_eq!(s.cost.tool_calls, 9);
        let clean = summarize(&trials(&[("a", &[1.0, 1.0]), ("b", &[0.0])]));
        assert_eq!(clean.contaminated, 0);
        assert_eq!(clean.trials, 3);
    }

    #[test]
    fn wilson_is_tighter_with_more_trials() {
        let (l6, h6) = wilson(5, 6);
        let (l60, h60) = wilson(50, 60);
        assert!(h6 - l6 > h60 - l60);
        assert!((0.0..50.0 / 60.0).contains(&l60) && (50.0 / 60.0..=1.0).contains(&h60));
    }

    #[test]
    fn same_score_twice_is_a_tie_and_kept() {
        let (low, _) = wilson(36, 60);
        assert_eq!(keep_decision(0.6, low, 0.6, low), Decision::Tie);
    }

    #[test]
    fn gain_inside_the_noise_follows_the_lower_bound() {
        let (low55, _) = wilson(33, 60);
        let (low60, _) = wilson(36, 60);
        assert_eq!(keep_decision(0.60, low60, 0.55, low55), Decision::Kept);
        assert_eq!(keep_decision(0.55, low55, 0.60, low60), Decision::Reverted);
    }

    #[test]
    fn clear_gain_is_kept() {
        let (low55, _) = wilson(33, 60);
        let (low70, _) = wilson(42, 60);
        assert_eq!(keep_decision(0.70, low70, 0.55, low55), Decision::Kept);
    }

    #[test]
    fn higher_point_on_fewer_trials_is_reverted() {
        let (low, _) = wilson(3, 3);
        let (best_low, _) = wilson(50, 60);
        assert_eq!(
            keep_decision(1.0, low, 50.0 / 60.0, best_low),
            Decision::Reverted
        );
    }

    #[test]
    fn one_unknown_or_overflowing_trial_invalidates_only_its_token_total() {
        let mut records = trials(&[("a", &[1.0, 0.0])]);
        records[1].input_tokens = None;
        records[0].output_tokens = Some(u64::MAX);
        let summary = summarize(&records);
        assert_eq!(summary.cost.input_tokens, None);
        assert_eq!(summary.cost.output_tokens, None);
        assert_eq!(summary.cost.cache_tokens, Some(0));
        assert_eq!(summary.score, 0.5);
        assert_eq!(summary.trials, 2);
    }

    #[test]
    fn summary_metrics() {
        let s = summarize(&trials(&[
            ("a", &[1.0, 1.0, 0.0]),
            ("b", &[0.0, 0.0, 0.0]),
            ("c", &[1.0, 0.0, 1.0]),
        ]));
        assert_eq!(s.trials, 9);
        assert_eq!(s.tasks, 3);
        assert!((s.score - 4.0 / 9.0).abs() < 1e-9);
        assert!((s.pass1 - (2.0 / 3.0 + 0.0 + 2.0 / 3.0) / 3.0).abs() < 1e-9);
        assert!((s.passk - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(s.cost.tool_calls, 27);
    }
}

"""The keep rule and the interval behind it."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from harness.iterate import keep_decision, summarize, wilson  # noqa: E402


def trials(task_rewards):
    out = []
    for task, rewards in task_rewards.items():
        for r in rewards:
            out.append({"task": task, "reward": r, "input_tokens": 10, "output_tokens": 5,
                        "tool_calls": 3, "wall_seconds": 1.0})
    return out


def test_wilson_is_tighter_with_more_trials():
    low6, high6 = wilson(5, 6)
    low60, high60 = wilson(50, 60)
    assert high6 - low6 > high60 - low60
    assert 0 <= low60 < 50 / 60 < high60 <= 1


def test_same_score_twice_is_a_tie_and_kept():
    low, _ = wilson(36, 60)
    assert keep_decision(0.6, low, 0.6, low) == "tie"


def test_small_gain_inside_the_noise_is_reverted_when_lower_bound_drops():
    # 0.55 then 0.60 with n=60: the 0.60 run is kept (its lower bound is
    # higher); the reverse order, 0.60 then 0.55, is reverted.
    low55, _ = wilson(33, 60)
    low60, _ = wilson(36, 60)
    assert keep_decision(0.60, low60, 0.55, low55) == "kept"
    assert keep_decision(0.55, low55, 0.60, low60) == "reverted"


def test_clear_gain_is_kept():
    low55, _ = wilson(33, 60)
    low70, _ = wilson(42, 60)
    assert keep_decision(0.70, low70, 0.55, low55) == "kept"


def test_higher_point_with_lower_bound_is_reverted():
    # A higher mean on far fewer trials has a lower bound below the best's.
    low, _ = wilson(3, 3)
    best_low, _ = wilson(50, 60)
    assert keep_decision(1.0, low, 50 / 60, best_low) == "reverted"


def test_summary_pass_metrics():
    s = summarize(trials({"a": [1.0, 1.0, 0.0], "b": [0.0, 0.0, 0.0], "c": [1.0, 0.0, 1.0]}))
    assert s["trials"] == 9 and s["tasks"] == 3
    assert abs(s["score"] - 4 / 9) < 1e-9
    assert abs(s["pass1"] - (2 / 3 + 0 + 2 / 3) / 3) < 1e-9
    assert abs(s["passk"] - 2 / 3) < 1e-9
    assert s["cost"]["tool_calls"] == 27

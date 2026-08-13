//! §7.2 The scorer: [`ToolSignals`] in, one signed number out.
//!
//! Positive is *capable*, negative is *efficient*, and `confidence` is the
//! magnitude. Per §7.3 the sign is a **ranking**, not a decision: with quota
//! headroom every turn starts at leg 0 and the score moves nothing. It only
//! chooses which turns to shed once headroom is scarce.
//!
//! Pure and total — no allocation, no clock, no I/O, no failure mode.
//!
//! # Attribution (§14)
//!
//! Derived from NVIDIA-NeMo/Switchyard <https://github.com/NVIDIA-NeMo/Switchyard>,
//! `crates/libsy/src/algorithms/util/stage.rs` (lines 35-47 and 244-321), read at
//! branch `main` on 2026-08-11 (no upstream commit hash was recorded). Copyright
//! (c) NVIDIA CORPORATION & AFFILIATES. Licensed under the Apache License,
//! Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0>; distributed on an
//! "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND. The four
//! constants below and the combining formula are the derived material; see the
//! top-level `NOTICE`. The rest of this file is original and MIT.

use super::signals::ToolSignals;

/// Weight of one maxed signal, pre-gain.
const SIGNAL_UNIT: f64 = 0.10;
/// Pre-`tanh` gain. `SIGNAL_UNIT * SCORE_GAIN = 0.5`, which is what puts one
/// maxed signal at `tanh(0.5)` — see `tests::one_maxed_signal_scores_tanh_half`.
const SCORE_GAIN: f64 = 5.0;
/// Normalizes severity to one unit: a *hard* error is worth exactly one signal,
/// a soft one 3/7 of a signal, a critical one 10/7 (and also trips
/// [`should_escalate`] outright).
const HARD_SEVERITY: f64 = 0.7;
/// Below this, a turn that has not written anything is just early. Above it,
/// the same turn is stalling. §7.8: this is a message count, so it means
/// something different per wire format — gate on it loosely.
const STALL_MIN_TURN_DEPTH: u32 = 8;

/// The scorer's verdict.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    /// Signed and bounded to `(-1.0, 1.0)`: positive leans capable, negative
    /// leans efficient.
    pub score: f64,
    /// `score.abs()`. Compared against `confidence_threshold` (§7.6) to decide
    /// whether the score is decisive enough to act on.
    pub confidence: f64,
}

/// Combine the signals into a single ranking number.
pub fn score(s: &ToolSignals) -> Score {
    let recent_ops =
        s.recent_write_count + s.recent_edit_count + s.recent_read_count + s.recent_todowrite_count;
    // §7.8: a conversation with no tool history at all is a plain chat client, not
    // a stalled agent — it must score exactly 0.0 and land on leg 0 every time. A
    // 20-turn conversation is otherwise indistinguishable from an agent that
    // stopped calling tools, and `spinning` would fire on it.
    let deep = s.has_tool_history && s.turn_depth >= STALL_MIN_TURN_DEPTH;
    let no_prod = s.recent_write_count == 0 && s.recent_edit_count == 0;
    let investig = s.recent_read_count >= 1 || s.recent_todowrite_count >= 1;

    // spinning and exploring partition the not-producing case, so at most one
    // fires — no double-counting on the production axis.
    let spinning = flag(deep && no_prod && !investig);
    let exploring = flag(deep && no_prod && investig);
    let production = ratio(s.recent_write_count + s.recent_edit_count, recent_ops);

    let raw =
        SIGNAL_UNIT * (f64::from(s.severity) / HARD_SEVERITY + spinning + exploring - production);
    let score = (SCORE_GAIN * raw).tanh(); // signed: + capable, - efficient
    Score {
        score,
        confidence: score.abs(),
    }
}

/// → capable, unconditionally. Checked before the score (§7.7 rung 3), so
/// neither term has to clear `confidence_threshold`.
///
/// `compacted` is meant to be **self-latching**: once an agent's context has been
/// summarized the task belongs on the capable tier and must not snap back the
/// moment the summary wipes the accumulated signals — the compaction is precisely
/// the event that destroys the evidence for keeping it there. This function only
/// reads the flag; the latch that makes it survive later turns lives in the
/// Phase 2 session store, not here.
pub fn should_escalate(s: &ToolSignals) -> bool {
    s.compacted || s.severity >= 1.0
}

/// → efficient, unconditionally: a settled run. All three terms are required —
/// tests pass, the agent actually produced something for them to pass against,
/// and nothing in the recent window went wrong. A green test suite with no
/// recent edit is just a repeated `cargo test`, not a finished task.
pub fn should_deescalate(s: &ToolSignals) -> bool {
    s.tests_passed && (s.recent_write_count + s.recent_edit_count) >= 1 && s.severity == 0.0
}

/// The spec's `as u8 as f64` on a `bool`, spelled without the cast chain.
fn flag(b: bool) -> f64 {
    f64::from(u8::from(b))
}

/// 0.0 when the denominator is 0 — an agent with no recent ops has no production
/// ratio, and dividing would make it `NaN`, which `tanh` would propagate all the
/// way into the routing decision.
fn ratio(numerator: u32, denominator: u32) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        f64::from(numerator) / f64::from(denominator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Signals with everything at rest, so each test names only what it varies.
    /// Baseline for a conversation that IS an agent run — every test below is
    /// about how an agent's signals score, so tool history is the premise. The
    /// no-tool-history case gets its own test.
    fn s() -> ToolSignals {
        ToolSignals {
            has_tool_history: true,
            ..ToolSignals::default()
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn a_plain_chat_scores_exactly_zero_at_any_depth() {
        // §7.8: "a pure chat client has no tool history, scores 0.0, and lands on
        // leg 0 every time." A 20-turn conversation with no tools is otherwise
        // indistinguishable from an agent that stopped calling them, and would
        // trip `spinning` — which the replay fixtures caught doing exactly that.
        for depth in [0, 4, 8, 20, 200] {
            let sig = ToolSignals {
                turn_depth: depth,
                has_tool_history: false,
                ..ToolSignals::default()
            };
            let got = score(&sig);
            assert_eq!(got.score, 0.0, "depth {depth} must not read as a stall");
            assert_eq!(got.confidence, 0.0);
        }
        // The same depth WITH tool history is a genuine stall.
        let stalled = ToolSignals {
            turn_depth: 20,
            has_tool_history: true,
            ..ToolSignals::default()
        };
        assert!(
            score(&stalled).score > 0.0,
            "a real agent stall still fires"
        );
    }

    #[test]
    fn one_maxed_signal_scores_tanh_half() {
        // The published calibration, and the reason `confidence_threshold = 0.5`
        // is a meaningful default: one maxed signal lands *below* it, so a single
        // noisy signal cannot flip a route on its own. Change any of
        // SIGNAL_UNIT / SCORE_GAIN / HARD_SEVERITY and this number moves, taking
        // the meaning of every configured threshold with it.
        let sig = ToolSignals {
            severity: 0.7, // exactly one unit
            ..s()
        };
        let got = score(&sig);
        assert!(
            close(got.score, 0.462),
            "one maxed signal must be tanh(0.5) = 0.462, got {}",
            got.score
        );
        assert!(close(got.confidence, 0.462));
        assert!(
            got.confidence < 0.5,
            "one signal must not clear the default"
        );
    }

    #[test]
    fn two_corroborating_signals_score_tanh_one() {
        // Two signals clear 0.5, one does not — which is what makes
        // `confidence_threshold = 0.5` mean "about one-and-a-half signals".
        let sig = ToolSignals {
            severity: 0.7,
            turn_depth: 8, // deep, nothing produced, nothing investigated
            ..s()
        };
        let got = score(&sig);
        assert!(
            close(got.score, 0.762),
            "two corroborating signals must be tanh(1.0) = 0.762, got {}",
            got.score
        );
        assert!(got.confidence > 0.5, "two signals must clear the default");
    }

    #[test]
    fn no_signals_score_exactly_zero() {
        // §7.8: a plain chat client has no tool history. Zero score, zero
        // confidence, nothing to act on, leg 0.
        let got = score(&s());
        assert_eq!(got.score, 0.0);
        assert_eq!(got.confidence, 0.0);
    }

    #[test]
    fn spinning_and_exploring_are_mutually_exclusive() {
        // Both are the "deep and not producing" case; only the presence of
        // investigation separates them. If both could fire the stall axis would
        // count twice and a stalled turn would score like two real signals.
        let spinning = score(&ToolSignals {
            turn_depth: 12,
            ..s()
        });
        let exploring = score(&ToolSignals {
            turn_depth: 12,
            recent_read_count: 3,
            ..s()
        });
        // One unit each, never two.
        assert!(close(spinning.score, 0.462), "{}", spinning.score);
        assert!(close(exploring.score, 0.462), "{}", exploring.score);
    }

    #[test]
    fn no_stall_below_the_minimum_turn_depth() {
        // Seven messages in, having written nothing is normal; eight is where it
        // starts to look like a stall.
        let shallow = score(&ToolSignals {
            turn_depth: STALL_MIN_TURN_DEPTH - 1,
            ..s()
        });
        assert_eq!(shallow.score, 0.0);
        let deep = score(&ToolSignals {
            turn_depth: STALL_MIN_TURN_DEPTH,
            ..s()
        });
        assert!(close(deep.score, 0.462), "{}", deep.score);
    }

    #[test]
    fn production_pulls_the_score_negative() {
        // An agent whose recent ops are all edits is producing: it needs no help,
        // so it is the first thing to shed under quota pressure.
        let got = score(&ToolSignals {
            recent_write_count: 1,
            recent_edit_count: 1,
            ..s()
        });
        assert!(close(got.score, -0.462), "{}", got.score);
        assert!(close(got.confidence, 0.462), "confidence is unsigned");
    }

    #[test]
    fn a_mixed_ratio_pulls_proportionally() {
        // 1 of 4 recent ops produced → production = 0.25 → tanh(-0.125).
        let got = score(&ToolSignals {
            recent_edit_count: 1,
            recent_read_count: 2,
            recent_todowrite_count: 1,
            ..s()
        });
        assert!(close(got.score, (-0.125f64).tanh()), "{}", got.score);
        assert!(got.score < 0.0);
    }

    #[test]
    fn a_pure_read_deep_turn_pulls_positive() {
        // Deep, investigating, producing nothing: exploring fires, production is
        // 0, so the turn is worth the capable tier.
        let got = score(&ToolSignals {
            turn_depth: 20,
            recent_read_count: 3,
            recent_todowrite_count: 1,
            ..s()
        });
        assert!(got.score > 0.0);
        assert!(close(got.score, 0.462), "{}", got.score);
    }

    #[test]
    fn severity_scales_below_and_above_one_unit() {
        // Soft is 3/7 of a unit; critical is 10/7 of one.
        let soft = score(&ToolSignals {
            severity: 0.3,
            ..s()
        });
        assert!(
            close(soft.score, (0.5f64 * 0.3 / 0.7).tanh()),
            "{}",
            soft.score
        );
        let critical = score(&ToolSignals {
            severity: 1.0,
            ..s()
        });
        assert!(
            close(critical.score, (0.5f64 / 0.7).tanh()),
            "{}",
            critical.score
        );
        assert!(soft.score < critical.score);
    }

    #[test]
    fn the_score_stays_bounded_and_finite() {
        // tanh saturates, so no combination of signals can produce a score
        // outside (-1, 1) or a NaN that would poison the comparison in §7.6.
        let extreme = score(&ToolSignals {
            severity: 1.0,
            turn_depth: u32::MAX,
            ..s()
        });
        assert!(extreme.score > 0.0 && extreme.score < 1.0);
        let saturated = score(&ToolSignals {
            recent_write_count: u32::MAX / 2,
            recent_edit_count: u32::MAX / 2,
            ..s()
        });
        assert!(saturated.score.is_finite());
        assert!(saturated.score > -1.0 && saturated.score < 0.0);
    }

    #[test]
    fn ratio_is_zero_when_the_denominator_is_zero() {
        assert_eq!(ratio(0, 0), 0.0);
        assert_eq!(ratio(5, 0), 0.0);
        assert_eq!(ratio(1, 4), 0.25);
        // And nothing upstream can produce a NaN through it.
        assert!(score(&s()).score.is_finite());
    }

    #[test]
    fn should_escalate_truth_table() {
        assert!(!should_escalate(&s()));
        assert!(should_escalate(&ToolSignals {
            compacted: true,
            ..s()
        }));
        assert!(should_escalate(&ToolSignals {
            severity: 1.0,
            ..s()
        }));
        // The boundary: hard is not critical.
        assert!(!should_escalate(&ToolSignals {
            severity: 0.7,
            ..s()
        }));
        // Compaction alone is enough even on an otherwise perfect run.
        assert!(should_escalate(&ToolSignals {
            compacted: true,
            tests_passed: true,
            recent_edit_count: 3,
            ..s()
        }));
    }

    #[test]
    fn should_deescalate_truth_table() {
        let settled = ToolSignals {
            tests_passed: true,
            recent_edit_count: 1,
            ..s()
        };
        assert!(should_deescalate(&settled));
        assert!(should_deescalate(&ToolSignals {
            tests_passed: true,
            recent_write_count: 1,
            ..s()
        }));
        // Each term is load-bearing.
        assert!(!should_deescalate(&ToolSignals {
            tests_passed: false,
            ..settled.clone()
        }));
        assert!(
            !should_deescalate(&ToolSignals {
                recent_edit_count: 0,
                ..settled.clone()
            }),
            "green tests with nothing produced is a repeated `cargo test`"
        );
        assert!(!should_deescalate(&s()));
    }

    #[test]
    fn should_deescalate_requires_a_spotless_window() {
        // The boundary: even the weakest error keeps a run from reading as
        // settled, because severity is a *windowed max* — a soft hit means
        // something went wrong within the last few results.
        for severity in [0.3, 0.7, 1.0] {
            assert!(
                !should_deescalate(&ToolSignals {
                    tests_passed: true,
                    recent_edit_count: 2,
                    severity,
                    ..s()
                }),
                "severity {severity} must block de-escalation"
            );
        }
    }

    #[test]
    fn the_overrides_cannot_both_fire() {
        // should_escalate is checked first (§7.7 rung 3), but the two are also
        // disjoint by construction: escalation needs compaction or severity 1.0,
        // de-escalation needs severity 0.0 and says nothing about compaction —
        // so only a compacted-but-clean run can satisfy both, and rung order
        // resolves it toward capable.
        let compacted_and_settled = ToolSignals {
            compacted: true,
            tests_passed: true,
            recent_edit_count: 1,
            ..s()
        };
        assert!(should_escalate(&compacted_and_settled));
        assert!(should_deescalate(&compacted_and_settled));
        let critical_and_settled = ToolSignals {
            severity: 1.0,
            tests_passed: true,
            recent_edit_count: 1,
            ..s()
        };
        assert!(should_escalate(&critical_and_settled));
        assert!(
            !should_deescalate(&critical_and_settled),
            "severity makes these mutually exclusive"
        );
    }
}

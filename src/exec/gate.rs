//! §8: acceptance gates.
//!
//! V1's chain advanced on failure and on exactly one quality condition —
//! `res.content.trim().is_empty() && !backend.allow_empty()`. That was already an
//! acceptance predicate; it was just hardcoded and singular. Generalize it and
//! escalation falls out for free: a leg that dispatches successfully must also
//! pass the chain's gate, or it is rejected and the chain advances down the same
//! path an empty result already takes.
//!
//! Why this matters: today a *confidently wrong* answer from a cheap model wins
//! the chain and is served. For an `improve` router over
//! `["gemini-flash", "gpt-oss", "deepseek-flash"]` that is the expected case, not
//! the edge case.

use crate::backend::{Backend, DispatchResult};
use crate::config::GateCfg;
use crate::exec::chain::ActiveSlot;
use crate::http::State;
use crate::log;
use crate::route::session::SessionKey;
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    /// Reject with a reason, which becomes the chain error for this leg so
    /// `/v1/backends` and the 503 body can say *why* a leg that answered was
    /// passed over.
    Reject(String),
}

/// What a gate is allowed to look at.
pub struct GateCtx<'a> {
    pub state: &'a State,
    pub backend: &'a Backend,
    pub backend_name: &'a str,
    /// The prompt the leg was given, so a judge can rate the answer against the
    /// actual request rather than in a vacuum.
    pub prompt: &'a str,
    pub session: Option<&'a SessionKey>,
    pub reqid: &'a str,
}

pub trait Gate: Send + Sync {
    fn check(&self, result: &DispatchResult, ctx: &GateCtx) -> Verdict;
}

/// V1's behavior, now explicit and named.
pub struct NonEmptyGate;

impl Gate for NonEmptyGate {
    fn check(&self, result: &DispatchResult, ctx: &GateCtx) -> Verdict {
        if result.content.trim().is_empty() && !ctx.backend.allow_empty() {
            return Verdict::Reject("empty output".into());
        }
        Verdict::Accept
    }
}

/// §8.1: ask a cheap judge whether the completed turn is good enough.
///
/// The judge rates **work actually done**, not a prediction about work that might
/// be done — which is the whole reason this is a gate on the result rather than
/// another input to routing.
///
/// Cost model, stated plainly because it is not obvious: a declined turn costs
/// cheap-leg + judge; a confirming turn costs cheap-leg + judge + expensive-leg.
pub struct JudgeGate {
    pub classifier: String,
    pub prompt: Option<String>,
    /// How many *consecutive* escalate verdicts before we actually escalate.
    /// Defaults to 2 — a single judge opinion is noisy, and escalating on it
    /// doubles the cost of every turn the judge happens to dislike.
    pub confirmations: u32,
    /// How much of the reply the judge sees (tail-truncated).
    pub window_chars: usize,
}

impl JudgeGate {
    pub fn from_config(cfg: &GateCfg) -> JudgeGate {
        JudgeGate {
            classifier: cfg.classifier.clone(),
            prompt: cfg.prompt.clone(),
            confirmations: cfg.confirmations.max(1),
            window_chars: cfg.window_chars,
        }
    }

    /// `confirmations > 1` needs somewhere to keep the streak. Without a session
    /// key there is nowhere, and the gate degrades to `confirmations = 1` rather
    /// than silently never escalating — which is the failure mode that would look
    /// exactly like the gate working.
    fn effective_confirmations(&self, session: Option<&SessionKey>) -> u32 {
        if session.is_none() {
            1
        } else {
            self.confirmations
        }
    }
}

impl Gate for JudgeGate {
    fn check(&self, result: &DispatchResult, ctx: &GateCtx) -> Verdict {
        let needed = self.effective_confirmations(ctx.session);
        let Some(judge) = ctx.state.backends.get(&self.classifier) else {
            // Fail open: a missing judge must never turn into an escalation.
            log::warn(&format!(
                "{} gate: judge '{}' is not a defined backend; accepting",
                ctx.reqid, self.classifier
            ));
            return Verdict::Accept;
        };
        // U-20: the judge is a backend like any other and must honor its cap.
        // Without this it escapes `max_concurrent` entirely — and a gate fires on
        // *every* accepted turn, so it is the single busiest caller in the tree.
        // At capacity, decline to judge rather than queue: the gate fails open by
        // design, and blocking here would stall a response that is already ready.
        let _slot = match judge.max_concurrent() {
            Some(max) => match ActiveSlot::try_acquire(&ctx.state.active, &self.classifier, max) {
                Some(slot) => Some(slot),
                None => {
                    log::debug(&format!(
                        "{} gate: judge '{}' at capacity; accepting",
                        ctx.reqid, self.classifier
                    ));
                    return Verdict::Accept;
                }
            },
            None => None,
        };
        let answer = tail(&result.content, self.window_chars);
        let prompt = match &self.prompt {
            Some(tpl) => tpl
                .replace("{input}", ctx.prompt)
                .replace("{output}", &answer),
            None => format!(
                "You are reviewing one completed turn from an AI assistant.\n\nRequest:\n{}\n\nAnswer:\n{answer}\n\nIs this answer good enough to serve as-is? Reply with exactly one word: ACCEPT if it is, ESCALATE if it is wrong, incomplete, or would not satisfy the request.",
                tail(ctx.prompt, self.window_chars)
            ),
        };
        let raw = serde_json::json!({"messages": [{"role": "user", "content": prompt}]});
        let timeout = Duration::from_secs(ctx.state.cfg.server.request_timeout_secs.min(60));
        let start = Instant::now();
        let reply = judge.dispatch(&prompt, &raw, &ctx.state.env, timeout);
        let ms = start.elapsed().as_millis() as u64;

        let verdict = match &reply {
            Ok(r) => parse_judge(&r.content),
            Err(e) => {
                log::warn(&format!(
                    "{} gate: judge failed ({e}); accepting",
                    ctx.reqid
                ));
                None
            }
        };
        ctx.state.stats.record_classifier(ms, verdict.is_none());

        match verdict {
            // **Judge failure fails open, and HOLDS the streak** rather than
            // clearing it. A judge outage must never create a latch, and must
            // never throw away accumulated evidence either.
            None => Verdict::Accept,
            Some(false) => {
                // Declined: the answer is fine. Reset the streak — the evidence
                // has to be consecutive to mean anything.
                if let Some(key) = ctx.session {
                    ctx.state.sessions.update(key, |st| st.escalate_streak = 0);
                }
                Verdict::Accept
            }
            Some(true) => {
                let streak = match ctx.session {
                    Some(key) => {
                        let mut n = 0;
                        ctx.state.sessions.update(key, |st| {
                            st.escalate_streak = st.escalate_streak.saturating_add(1);
                            n = st.escalate_streak;
                        });
                        n
                    }
                    None => 1,
                };
                if streak < needed {
                    // Not yet confirmed: serve the buffered reply and keep count.
                    log::info(&format!(
                        "{} gate: escalate {streak}/{needed} on {}; serving anyway",
                        ctx.reqid, ctx.backend_name
                    ));
                    return Verdict::Accept;
                }
                // Confirmed. Latch the session so later turns skip both the cheap
                // leg and the judge — there is no point re-paying for a verdict
                // this conversation has already reached twice.
                if let Some(key) = ctx.session {
                    ctx.state.sessions.update(key, |st| {
                        st.escalate_streak = 0;
                        st.latched = Some(ctx.backend_name.to_string());
                    });
                }
                Verdict::Reject(format!("judge escalated ({streak}/{needed} confirmations)"))
            }
        }
    }
}

/// `Some(true)` = escalate, `Some(false)` = decline, `None` = unusable.
///
/// An unusable reply is not a decline: §8 says a judge outage must fail open
/// *and hold the streak*, and lumping "the judge said no" together with "the
/// judge said nothing" would quietly clear accumulated evidence.
fn parse_judge(reply: &str) -> Option<bool> {
    let r = reply.trim().to_ascii_lowercase();
    let esc = r.contains("escalate");
    let acc = r.contains("accept");
    match (esc, acc) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        // Both words, or neither: we cannot tell what it meant.
        _ => None,
    }
}

fn tail(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    chars[chars.len() - max..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn judge_replies_are_read_conservatively() {
        assert_eq!(parse_judge("ESCALATE"), Some(true));
        assert_eq!(parse_judge("  accept \n"), Some(false));
        assert_eq!(parse_judge("I would escalate this one"), Some(true));
        // Both words is not a verdict — the reply is unusable, and §8 says an
        // unusable verdict fails open WITHOUT clearing the streak.
        assert_eq!(parse_judge("accept or escalate?"), None);
        assert_eq!(parse_judge("hmm"), None);
        assert_eq!(parse_judge(""), None);
    }

    #[test]
    fn a_missing_session_degrades_confirmations_to_one() {
        // §8: confirmations > 1 needs somewhere to keep the streak. Silently
        // never escalating would look exactly like the gate working.
        let g = JudgeGate {
            classifier: "j".into(),
            prompt: None,
            confirmations: 3,
            window_chars: 500,
        };
        assert_eq!(g.effective_confirmations(None), 1);
        assert_eq!(
            g.effective_confirmations(Some(&SessionKey::Header("s".into()))),
            3
        );
    }

    #[test]
    fn confirmations_zero_is_clamped_to_one() {
        // A gate that escalates on zero confirmations would escalate before the
        // judge ever spoke.
        let g = JudgeGate::from_config(&GateCfg {
            kind: "judge".into(),
            classifier: "j".into(),
            prompt: None,
            confirmations: 0,
            window_chars: 500,
        });
        assert_eq!(g.confirmations, 1);
    }

    #[test]
    fn the_window_keeps_the_tail_of_a_long_answer() {
        // The end of an answer is where a model gives up, contradicts itself, or
        // trails off — which is what a judge needs to see.
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("ab", 5), "ab");
        assert_eq!(tail(&"\u{1F600}".repeat(10), 3).chars().count(), 3);
    }
}

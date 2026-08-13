//! Policies: the things that turn a request into a [`Decision`].
//!
//! Selection happens once at startup (`State::build_policies`), not per request —
//! a `[routers.x]` table builds a [`ClassifierPolicy`], everything else uses the
//! shared [`StaticPolicy`].

use super::session::StoredDecision;
use super::signals::ToolSignals;
use super::{Decision, DecisionSource, RequestCtx, RouteError, resolve, scorer, signals};
use crate::backend::BackendError;
use crate::config::{Config, Router};
use crate::exec::chain::{ActiveSlot, outcome_for};
use crate::http::State;
use crate::log;
use crate::proto::openai;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Synchronous, `&self`, no allocation in the hot path beyond the returned
/// `Decision`.
pub trait Policy: Send + Sync {
    fn decide(&self, ctx: &RequestCtx, state: &State) -> Result<Decision, RouteError>;
}

/// V1's `resolve` branch: the model names a chain, a backend, or nothing we know.
pub struct StaticPolicy;

impl Policy for StaticPolicy {
    fn decide(&self, ctx: &RequestCtx, _state: &State) -> Result<Decision, RouteError> {
        let known =
            ctx.cfg.chains.contains_key(ctx.model) || ctx.cfg.backends.contains_key(ctx.model);
        if !known && ctx.cfg.server.strict_models {
            // U-3: an unknown model is a client error, not a silent redirect.
            return Err(RouteError {
                status: 404,
                message: format!(
                    "unknown model '{}' — list valid ids at GET /v1/models, or set strict_models = false to fall back to default_chain",
                    ctx.model
                ),
                etype: "invalid_request_error",
                code: "model_not_found",
            });
        }
        let legs = resolve::resolve(ctx.model, ctx.cfg).map_err(|e| RouteError {
            status: 400,
            message: e,
            etype: "invalid_request_error",
            code: "bad_model",
        })?;
        let (source, rationale) = if known {
            (
                DecisionSource::Static,
                format!("model '{}' names a chain or backend", ctx.model),
            )
        } else {
            (
                DecisionSource::Default,
                format!(
                    "unknown model '{}' -> default_chain '{}'",
                    ctx.model, ctx.cfg.server.default_chain
                ),
            )
        };
        Ok(Decision {
            legs: legs.iter().map(|s| s.to_string()).collect(),
            source,
            rationale,
            label: None,
            chain: ctx
                .cfg
                .chains
                .contains_key(ctx.model)
                .then(|| ctx.model.to_string()),
        })
    }
}

/// §5's auto-router: ask a classifier backend to label the task, then route the
/// class to its chain.
pub struct ClassifierPolicy {
    /// Names a `[routers.<name>]` table. Held by name rather than by reference so
    /// the policy outlives no borrow of `Config` — which is what lets U-22 swap
    /// `Arc<State>` on SIGHUP without rebuilding lifetimes.
    pub router: String,
}

impl Policy for ClassifierPolicy {
    fn decide(&self, ctx: &RequestCtx, state: &State) -> Result<Decision, RouteError> {
        let Some(router_cfg) = ctx.cfg.routers.get(&self.router) else {
            // Unreachable via State::build_policies, which only creates a
            // ClassifierPolicy for a router that exists. Reachable after a
            // future SIGHUP reload races a policy map; answer, do not panic.
            return Err(router_error(format!("router '{}' is gone", self.router)));
        };
        // §6.3, rung 1 of the §7.7 cascade: an affinity hit skips the classifier
        // call entirely. That skip is the whole cost win — on turn 12 of an agent
        // run we would otherwise pay a round-trip to classify "yes, continue".
        if router_cfg.session_affinity
            && let Some(key) = ctx.session
            && let Some(prev) = state
                .sessions
                .get(key)
                .and_then(|st| st.decision)
                .filter(|d| d.router == self.router)
        {
            log::debug(&format!(
                "{} route {} affinity session={} legs={}",
                ctx.reqid,
                ctx.model,
                key.label(),
                prev.legs.join(",")
            ));
            return Ok(Decision {
                legs: prev.legs,
                source: DecisionSource::Affinity,
                rationale: format!(
                    "session affinity: reusing this conversation's {} decision",
                    prev.source.as_str()
                ),
                label: prev.label,
                chain: prev.chain,
            });
        }
        let (class, source, classifier_ms) = classify_task(state, router_cfg, ctx.messages);
        let Some(target) = router_cfg.routes.get(&class).cloned() else {
            return Err(router_error(format!(
                "router '{}' has no route for class '{class}'",
                self.router
            )));
        };
        let legs = resolve::resolve(&target, ctx.cfg).map_err(router_error)?;
        log::info(&format!(
            "{} route {} class={class} target={target} source={} classifier_ms={classifier_ms}",
            ctx.reqid,
            ctx.model,
            source.as_str()
        ));
        let rationale = match source {
            DecisionSource::Classifier => {
                format!("classifier chose '{class}' in {classifier_ms}ms -> {target}")
            }
            _ => format!("classifier unusable -> fallback class '{class}' -> {target}"),
        };
        let legs: Vec<String> = legs.iter().map(|s| s.to_string()).collect();
        let label = Some(format!("{class}/{target}"));
        // §6.3: store a FallOpen decision too. Otherwise a flaky classifier gets
        // re-hammered every single turn, which is the opposite of the point.
        if router_cfg.session_affinity
            && let Some(key) = ctx.session
        {
            let stored = StoredDecision {
                router: self.router.clone(),
                legs: legs.clone(),
                source,
                label: label.clone(),
                chain: ctx.cfg.chains.contains_key(&target).then(|| target.clone()),
            };
            state
                .sessions
                .update(key, |st| st.decision = Some(stored.clone()));
        }
        Ok(Decision {
            legs,
            source,
            rationale,
            label,
            chain: ctx.cfg.chains.contains_key(&target).then_some(target),
        })
    }
}

/// §7 — the signal router.
///
/// The scorer's role here is **ranker, not decider**. Quota pressure sets *how
/// many* turns get the flat-rate backend; the score only chooses *which* ones,
/// and only once headroom is actually scarce. With pressure below
/// `reserve_threshold` the whole cascade is one comparison and a return (rung 4),
/// which is the property that makes this safe to enable by default.
pub struct SignalPolicy {
    pub router: String,
}

impl Policy for SignalPolicy {
    fn decide(&self, ctx: &RequestCtx, state: &State) -> Result<Decision, RouteError> {
        let Some(r) = ctx.cfg.routers.get(&self.router) else {
            return Err(router_error(format!("router '{}' is gone", self.router)));
        };
        // NOTE ON RUNG 1. §7.7 lists session affinity first, and a classifier
        // router takes it literally — there, affinity's whole job is to skip an
        // LLM round-trip, and the decision it replays does not depend on
        // anything that changes between turns.
        //
        // A signals router is different, and replaying a whole decision here
        // would be a bug: rungs 2 through 6 are functions of *live quota
        // pressure* and *this turn's* tool signals. Caching one would mean a
        // session that hit the wall once stays shed forever after the window
        // resets and — worse — a session that started with headroom keeps
        // spending leg 0 long after the quota is gone, defeating the whole
        // mechanism. Affinity therefore applies to exactly the rung with a
        // round-trip to save: the classifier consultation at rung 7.
        let chain_name = r.chain.as_deref().unwrap_or(&ctx.cfg.server.default_chain);
        let legs: Vec<String> = resolve::resolve(chain_name, ctx.cfg)
            .map_err(router_error)?
            .iter()
            .map(|s| s.to_string())
            .collect();
        let label = Some(format!("signals/{chain_name}"));
        let leg0 = legs.first().cloned().unwrap_or_default();

        // §7.5: an unknown limit yields no pressure, so an unconfigured server
        // behaves exactly as it does today until it hits a 429.
        let pressure = state.stats.pressure(&leg0).unwrap_or(0.0);
        let sig = signals::extract(
            ctx.raw.get("messages").unwrap_or(&serde_json::Value::Null),
            r.recent_turn_window,
        );

        let gate_chain = ctx
            .cfg
            .chains
            .contains_key(chain_name)
            .then(|| chain_name.to_string());
        let finish = |legs: Vec<String>, source, rationale: String| {
            let d = Decision {
                legs,
                source,
                rationale,
                label: label.clone(),
                chain: gate_chain.clone(),
            };
            Ok(d)
        };

        // Rung 2: leg 0 is past the wall. Skipped, not cooled — the backend is
        // healthy, it is just out of quota until the window resets.
        if pressure >= 1.0 {
            if let Some(rest) = shed(&legs, &sig, ctx.cfg) {
                return finish(
                    rest,
                    DecisionSource::Quota,
                    format!("{leg0} is quota-exhausted; starting one leg deeper"),
                );
            }
            // Nothing agentic-safe deeper: better to try the exhausted leg and
            // get a real 429 than to hand a tool-running turn to a prose model.
            return finish(
                legs,
                DecisionSource::Quota,
                format!("{leg0} is quota-exhausted but no agentic-safe leg is deeper"),
            );
        }

        // Rung 3: a critical error or a compacted context. Unconditional.
        if scorer::should_escalate(&sig) {
            let why = if sig.compacted {
                "context was compacted"
            } else {
                "critical tool error"
            };
            return finish(
                legs,
                DecisionSource::Override,
                format!("{why}; forcing the flat-rate leg"),
            );
        }

        // Rung 4: headroom. The common path, and deliberately the cheapest one —
        // no shedding, no classifier, one comparison and a return.
        if pressure < r.reserve_threshold {
            return finish(
                legs,
                DecisionSource::Static,
                format!(
                    "quota pressure {pressure:.2} < reserve {:.2}; leg 0",
                    r.reserve_threshold
                ),
            );
        }

        // --- Rationing from here down. ---
        let sc = scorer::score(&sig);

        // Rung 5: a settled run — tests passed, work was produced, nothing broke.
        if scorer::should_deescalate(&sig)
            && let Some(rest) = shed(&legs, &sig, ctx.cfg)
        {
            return finish(
                rest,
                DecisionSource::Settled,
                format!("tests passed and no errors under pressure {pressure:.2}; shedding a leg"),
            );
        }

        // Rung 6: the scorer is decisive.
        if sc.confidence >= r.confidence_threshold {
            if sc.score > 0.0 {
                return finish(
                    legs,
                    DecisionSource::Dimensions,
                    format!(
                        "score {:+.2} (confidence {:.2}) says this turn is worth the quota",
                        sc.score, sc.confidence
                    ),
                );
            }
            if let Some(rest) = shed(&legs, &sig, ctx.cfg) {
                return finish(
                    rest,
                    DecisionSource::Dimensions,
                    format!(
                        "score {:+.2} (confidence {:.2}) under pressure {pressure:.2}; shedding a leg",
                        sc.score, sc.confidence
                    ),
                );
            }
        }

        // Rung 7: sub-threshold, so ask the classifier if one is configured.
        //
        // This is the one rung whose answer does NOT move with quota pressure, so
        // it is the one rung affinity applies to: replaying it skips the
        // round-trip §7 exists to avoid, while rungs 2-6 stay live above it.
        if r.classifier.is_some() {
            if r.session_affinity
                && let Some(key) = ctx.session
                && let Some(prev) = state
                    .sessions
                    .get(key)
                    .and_then(|st| st.decision)
                    .filter(|d| d.router == self.router && d.source == DecisionSource::Classifier)
            {
                return Ok(Decision {
                    legs: prev.legs,
                    source: DecisionSource::Affinity,
                    rationale: "session affinity: reusing this conversation's class".to_string(),
                    label: prev.label,
                    chain: prev.chain,
                });
            }
            let (class, source, ms) = classify_task(state, r, ctx.messages);
            if source == DecisionSource::Classifier
                && let Some(target) = r.routes.get(&class).cloned()
                && let Ok(names) = resolve::resolve(&target, ctx.cfg)
            {
                let d = Decision {
                    legs: names.iter().map(|s| s.to_string()).collect(),
                    source: DecisionSource::Classifier,
                    rationale: format!(
                        "score was indecisive; classifier chose '{class}' in {ms}ms"
                    ),
                    label: Some(format!("{class}/{target}")),
                    chain: ctx.cfg.chains.contains_key(&target).then_some(target),
                };
                store_affinity(state, ctx, &self.router, r, &d);
                return Ok(d);
            }
        }

        // Rung 8: nothing was decisive, and we are rationing — shed. §7.6: "a turn
        // whose score does not clear confidence_threshold on the capable side
        // starts one leg deeper, preserving the flat-rate leg."
        match shed(&legs, &sig, ctx.cfg) {
            Some(rest) => finish(
                rest,
                DecisionSource::FallOpen,
                format!("no decisive signal under pressure {pressure:.2}; shedding a leg"),
            ),
            None => finish(
                legs,
                DecisionSource::FallOpen,
                "no decisive signal and no agentic-safe leg deeper; staying at leg 0".to_string(),
            ),
        }
    }
}

/// §7.6's agentic constraint: start one leg deeper, but **never** shed a
/// tool-running turn to a text-only backend.
///
/// `codex` writing files is not substitutable by `or-sonnet` returning prose
/// *about* writing files — the request would "succeed" and return the wrong kind
/// of artifact entirely. Spending quota you meant to save is recoverable next
/// window; silently returning prose where the caller expected an edit is not.
/// Returns `None` when there is nothing safe to shed to, and the caller keeps
/// leg 0 regardless of pressure.
fn shed(legs: &[String], sig: &ToolSignals, cfg: &Config) -> Option<Vec<String>> {
    if legs.len() < 2 {
        return None;
    }
    if sig.recent_ops() == 0 {
        // No tool history: any deeper leg will do.
        return Some(legs[1..].to_vec());
    }
    let start = legs
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, n)| cfg.backends.get(*n).is_some_and(|b| b.is_agentic()))
        .map(|(i, _)| i)?;
    Some(legs[start..].to_vec())
}

/// §6.3: a signals decision is stored for affinity on the same terms a
/// classifier one is — including a fall-open, so a noisy turn is not re-scored
/// (and possibly re-classified) every turn of the conversation.
fn store_affinity(state: &State, ctx: &RequestCtx, router: &str, r: &Router, d: &Decision) {
    if !r.session_affinity {
        return;
    }
    let Some(key) = ctx.session else { return };
    let stored = StoredDecision {
        router: router.to_string(),
        legs: d.legs.clone(),
        source: d.source,
        label: d.label.clone(),
        chain: d.chain.clone(),
    };
    state
        .sessions
        .update(key, |st| st.decision = Some(stored.clone()));
}

fn router_error(message: String) -> RouteError {
    RouteError {
        status: 500,
        message,
        etype: "server_error",
        code: "router_error",
    }
}

/// §5: classify the task via the router's classifier backend. Never fails the
/// request — any error/timeout/garbage falls back to `fallback_class`, and every
/// such path reports `FallOpen` so the fall-open rate is visible in the logs and
/// in `/v1/stats`.
///
/// Returns the class, its provenance, and how long the classifier round-trip
/// took (0 when no call was made).
pub fn classify_task(
    state: &State,
    router_cfg: &Router,
    messages: &[openai::ChatMessage],
) -> (String, DecisionSource, u64) {
    let classes: Vec<&str> = router_cfg.routes.keys().map(|s| s.as_str()).collect();
    let truncated = classifier_input(
        messages,
        router_cfg.classifier_max_chars,
        state.cfg.server.classifier_include_opening,
    );
    let prompt = match &router_cfg.classifier_prompt {
        Some(tpl) => tpl
            .replace("{classes}", &classes.join(", "))
            .replace("{input}", &truncated),
        None => format!(
            "Classify the task into exactly one of these labels: {}.\n\nTask:\n{}\n\nAnswer with exactly one label.",
            classes.join(", "),
            truncated
        ),
    };
    let Some(classifier_name) = router_cfg.classifier.as_deref() else {
        return fall_open(router_cfg);
    };
    let Some(classifier) = state.backends.get(classifier_name) else {
        return fall_open(router_cfg);
    };
    // U-20: honor the classifier backend's concurrency cap. At capacity, degrade
    // to the fallback class rather than spawning an uncounted session (the same
    // cap run_chain enforces on chain legs). Routing never fails the request.
    let _slot = match classifier.max_concurrent() {
        Some(max) => match ActiveSlot::try_acquire(&state.active, classifier_name, max) {
            Some(slot) => Some(slot),
            None => return fall_open(router_cfg),
        },
        None => None,
    };
    let timeout = Duration::from_secs(router_cfg.classifier_timeout_secs);
    let raw = serde_json::json!({"messages": [{"role": "user", "content": prompt}]});
    let start = Instant::now();
    let result = classifier.dispatch(&prompt, &raw, &state.env, timeout);
    let ms = start.elapsed().as_millis() as u64;
    // §5.2: a classifier dispatch is a dispatch. Without this the backend behind
    // a router reports zero traffic and zero 429s in the very quota block that
    // exists to measure them. Health is deliberately NOT touched — cooling a
    // classifier is a routing behavior change, not observation, and routing
    // already degrades to the fallback class on its own.
    match &result {
        Ok(_) => state
            .stats
            .record_backend(classifier_name, crate::stats::Outcome::Success, ms),
        Err(e) => {
            state
                .stats
                .record_backend(classifier_name, outcome_for(e), ms);
            if let BackendError::RateLimited(_, after, _) = e {
                state.stats.record_rate_limited(
                    classifier_name,
                    Some(after.unwrap_or(Duration::from_secs(state.cfg.server.cooldown_secs))),
                );
            }
        }
    }
    let (class, source) = match result {
        Ok(r) => parse_class(&r.content, &router_cfg.routes, &fallback_class(router_cfg)),
        Err(_) => (fallback_class(router_cfg), DecisionSource::FallOpen),
    };
    state
        .stats
        .record_classifier(ms, source == DecisionSource::FallOpen);
    (class, source, ms)
}

/// A fall-open that never reached the classifier: no call, so no latency and
/// nothing for `/v1/stats` to average.
fn fall_open(router_cfg: &Router) -> (String, DecisionSource, u64) {
    (fallback_class(router_cfg), DecisionSource::FallOpen, 0)
}

/// `fallback_class` is optional in the struct because a `signals` router without
/// a classifier has no classes at all. `validate()` requires it wherever it is
/// actually consulted, so an empty string here is unreachable in a valid config
/// and would fall through to the "no route for class" error rather than panic.
fn fallback_class(router_cfg: &Router) -> String {
    router_cfg.fallback_class.clone().unwrap_or_default()
}

/// §4.1: map a classifier reply onto one of the router's classes.
///
/// Pass 1 is an exact match and is unambiguous by construction — map keys are
/// unique, so at most one can compare equal. Pass 2 is a substring match and only
/// commits when **exactly one** class matches: a reply like "this is a code task,
/// not a write task" hits both `code` and `write`, and which one won used to vary
/// between runs of the same binary because `HashMap`'s `RandomState` reseeds per
/// process. Ambiguity now falls open and says so.
pub fn parse_class(
    reply: &str,
    routes: &HashMap<String, String>,
    fallback: &str,
) -> (String, DecisionSource) {
    let r = reply.trim().to_ascii_lowercase();

    // Pass 1: exact match. Unambiguous by construction (keys are unique).
    if let Some(k) = routes.keys().find(|k| k.to_ascii_lowercase() == r) {
        return (k.clone(), DecisionSource::Classifier);
    }

    // Pass 2: substring — only when EXACTLY ONE class matches. Iterate a sorted
    // key list so a future change here can't reintroduce order-dependence.
    let mut keys: Vec<&String> = routes.keys().collect();
    keys.sort_unstable();
    let hits: Vec<&&String> = keys
        .iter()
        .filter(|k| r.contains(&k.to_ascii_lowercase()))
        .collect();
    match hits.as_slice() {
        [only] => ((**only).clone(), DecisionSource::Classifier),
        // 0 matches (nothing recognizable) or 2+ (ambiguous) both fall open.
        _ => (fallback.to_string(), DecisionSource::FallOpen),
    }
}

fn truncate_tail(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    chars[chars.len() - max..].iter().collect()
}

/// Mirror image of `truncate_tail`: keep the **first** `max` chars. Char-based,
/// not byte-based, so it cannot split a UTF-8 sequence.
fn truncate_head(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    chars[..max].iter().collect()
}

/// §4.2: build what the classifier is actually shown.
///
/// v0.2.0 tail-truncated the *latest* user message, which on turn 12 of an agent
/// run hands the classifier "yes, continue" — zero routing signal — and then logs
/// a confident class. Show it both ends of the conversation instead: the opening
/// task head-truncated (intent is at the start) plus the latest turn
/// tail-truncated (the ask is at the end), joined by a marker so the classifier
/// can tell which is current.
///
/// The separator is charged to the budget, so the result is always at most `max`
/// chars — which is what `classifier_max_chars` promises whoever set it. A short
/// opening donates its unused remainder to the latest turn (plan O2).
fn classifier_input(messages: &[openai::ChatMessage], max: usize, include_opening: bool) -> String {
    let latest = openai::last_user_text(messages);
    // The opt-out, and the degenerate single-turn case: tail-truncate the latest
    // turn over the whole budget. Byte-for-byte v0.2.0 behavior.
    if !include_opening || openai::user_message_count(messages) < 2 {
        return truncate_tail(&latest, max);
    }
    const SEP: &str = "\n\n[latest message]\n";
    let body = max.saturating_sub(SEP.chars().count());
    if body == 0 {
        // No room for both halves plus the marker; the latest turn is the safer
        // half to keep, and the `<= max` guarantee holds.
        return truncate_tail(&latest, max);
    }
    let head = truncate_head(&openai::first_user_text(messages), body * 60 / 100);
    // O2: whatever the opening did not use goes to the latest turn.
    let tail = truncate_tail(&latest, body - head.chars().count());
    format!("{head}{SEP}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the route table T1-T5 share, fresh each call so every `HashMap`
    /// gets its own `RandomState` seed.
    fn routes(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn msg(role: &str, text: &str) -> openai::ChatMessage {
        openai::ChatMessage {
            role: role.to_string(),
            content: openai::MessageContent::Text(text.to_string()),
            name: None,
        }
    }

    #[test]
    fn parse_class_exact_match_wins_over_substring() {
        // T1: `code` and `code-review` both hit on the substring pass, so the
        // exact match must short-circuit before it ever runs.
        let r = routes(&[("code", "a"), ("code-review", "b")]);
        assert_eq!(
            parse_class("code", &r, "code-review"),
            ("code".to_string(), DecisionSource::Classifier)
        );
    }

    #[test]
    fn parse_class_single_substring_match_is_classifier() {
        // T2: the v0.2.0 test, retyped for the tuple. Exactly one class appears
        // in the reply, so the substring pass still commits.
        let r = routes(&[("research", "best"), ("execution", "fast")]);
        assert_eq!(
            parse_class("RESEARCH", &r, "execution"),
            ("research".to_string(), DecisionSource::Classifier)
        );
        assert_eq!(
            parse_class("I think this is execution work", &r, "research"),
            ("execution".to_string(), DecisionSource::Classifier)
        );
    }

    #[test]
    fn parse_class_ambiguous_reply_falls_open_across_100_maps() {
        // T3: the nondeterminism this fix exists for. `RandomState` reseeds per
        // HashMap *construction*, so one map is a coin flip that passes ~50% of
        // the time against the old code — build a fresh one every iteration.
        for i in 0..100 {
            let r = routes(&[("code", "a"), ("write", "b")]);
            assert_eq!(
                parse_class("this is a code task, not a write task", &r, "general"),
                ("general".to_string(), DecisionSource::FallOpen),
                "iteration {i}: two classes matched, so the reply is unusable"
            );
        }
    }

    #[test]
    fn parse_class_no_match_falls_open() {
        // T4: zero matches is FallOpen, not Classifier — §3.1 insists the two are
        // different things, and V1 landed both in the same silent path.
        let r = routes(&[("research", "best"), ("execution", "fast")]);
        assert_eq!(
            parse_class("nonsense", &r, "execution"),
            ("execution".to_string(), DecisionSource::FallOpen)
        );
    }

    #[test]
    fn parse_class_is_case_and_whitespace_insensitive() {
        // T5: guards the trim().to_ascii_lowercase() the rewrite must preserve.
        let r = routes(&[("research", "best"), ("execution", "fast")]);
        assert_eq!(
            parse_class("  RESEARCH \n", &r, "execution"),
            ("research".to_string(), DecisionSource::Classifier)
        );
    }

    /// A 12-message agent run: a real opening task, then filler, ending on the
    /// content-free continuation that used to be all the classifier ever saw.
    fn agent_run() -> Vec<openai::ChatMessage> {
        let mut m = vec![msg("user", "Refactor the auth module to use JWTs")];
        for i in 0..5 {
            m.push(msg("assistant", &format!("step {i}")));
            m.push(msg("user", "ok"));
        }
        m.push(msg("user", "yes, continue"));
        m
    }

    #[test]
    fn classifier_input_combines_opening_and_latest() {
        // T6: the defect, stated as an assertion.
        let got = classifier_input(&agent_run(), 4000, true);
        assert!(
            got.contains("Refactor the auth module to use JWTs"),
            "opening task missing: {got}"
        );
        assert!(got.contains("yes, continue"), "latest turn missing: {got}");
    }

    #[test]
    fn classifier_input_respects_the_budget_and_the_60_40_split() {
        // T7: both halves far over budget. Asserts on the actual boundary chars,
        // not just lengths — a head/tail swap is the bug this fix exists to stop.
        let opening: String = "abcdefghij".repeat(20); // 200 chars
        let latest: String = "0123456789".repeat(20); // 200 chars
        let messages = vec![
            msg("user", &opening),
            msg("assistant", "..."),
            msg("user", &latest),
        ];
        const MAX: usize = 119; // 100 body + the 19-char separator
        let got = classifier_input(&messages, MAX, true);
        assert!(
            got.chars().count() <= MAX,
            "budget blown: {} > {MAX}",
            got.chars().count()
        );
        let (head, tail) = got.split_once("\n\n[latest message]\n").expect("separator");
        assert_eq!(head.chars().count(), 60, "head budget is 60% of the body");
        assert_eq!(tail.chars().count(), 40, "tail gets the remainder");
        assert!(
            head.starts_with("abcdefghij"),
            "opening must be HEAD-truncated: {head}"
        );
        assert!(
            tail.ends_with("0123456789"),
            "latest must be TAIL-truncated: {tail}"
        );
    }

    #[test]
    fn classifier_input_single_user_message_gets_the_whole_budget() {
        // T8: one user message -> v0.2.0 behavior, no separator.
        let only = "x".repeat(500);
        let messages = vec![msg("system", "be brief"), msg("user", &only)];
        let got = classifier_input(&messages, 100, true);
        assert_eq!(got, truncate_tail(&only, 100));
        assert!(!got.contains("[latest message]"), "no separator: {got}");
    }

    #[test]
    fn classifier_include_opening_false_reproduces_v0_2_0() {
        // T9: the opt-out is byte-for-byte, which is the promise the key makes.
        let messages = agent_run();
        assert_eq!(
            classifier_input(&messages, 40, false),
            truncate_tail(&openai::last_user_text(&messages), 40)
        );
    }

    #[test]
    fn classifier_input_is_char_safe_on_multibyte_input() {
        // T10: truncate_head is index arithmetic; a byte-slice mistake would
        // panic here, or split a 4-byte sequence into invalid UTF-8.
        let emoji = "\u{1F600}".repeat(100);
        let messages = vec![
            msg("user", &emoji),
            msg("assistant", "…"),
            msg("user", &emoji),
        ];
        let got = classifier_input(&messages, 60, true);
        assert!(got.chars().count() <= 60);
        let (head, tail) = got.split_once("\n\n[latest message]\n").expect("separator");
        // Every surviving char is a whole emoji — no sequence was split.
        assert!(head.chars().all(|c| c == '\u{1F600}'), "head: {head:?}");
        assert!(tail.chars().all(|c| c == '\u{1F600}'), "tail: {tail:?}");
        // The head path alone, on a budget that lands mid-sequence.
        assert_eq!(truncate_head(&emoji, 3).chars().count(), 3);
    }
}

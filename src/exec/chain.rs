//! `run_chain` and the per-backend health it reads and writes.

use crate::backend::{BackendError, DispatchResult};
use crate::config::GateCfg;
use crate::exec::gate::{Gate, GateCtx, JudgeGate, NonEmptyGate, Verdict};
use crate::http::State;
use crate::log;
use crate::proto::PromptMode;
use crate::route::session::SessionKey;
use crate::stats::Outcome;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// P-9 / U-13: per-backend health, updated after each dispatch.
#[derive(Default, Clone)]
pub struct BackendHealth {
    pub consecutive_failures: u32,
    pub cooling_until: Option<Instant>,
    pub last_error: Option<String>,
    pub last_latency_ms: Option<u64>,
}

/// U-20: RAII guard for a `state.active` concurrency slot. `try_acquire`
/// increments the in-flight count under the backend's cap (returning `None` when
/// already at the cap); `Drop` decrements it. Because the server is built to
/// survive dispatch panics (F-3 `catch_unwind` in `serve`), this guard is what
/// keeps a panic between acquire and release from leaking the slot forever — one
/// leak would brick a `max_concurrent = 1` backend until restart.
pub struct ActiveSlot<'a> {
    active: &'a Mutex<HashMap<String, usize>>,
    name: String,
}

impl<'a> ActiveSlot<'a> {
    /// Reserve one slot for `name` under `max`. `None` if already at capacity.
    pub fn try_acquire(
        active: &'a Mutex<HashMap<String, usize>>,
        name: &str,
        max: usize,
    ) -> Option<Self> {
        let mut counts = active.lock().unwrap();
        let count = counts.entry(name.to_string()).or_insert(0);
        if *count >= max {
            return None;
        }
        *count += 1;
        Some(Self {
            active,
            name: name.to_string(),
        })
    }
}

impl Drop for ActiveSlot<'_> {
    fn drop(&mut self) {
        if let Ok(mut counts) = self.active.lock()
            && let Some(c) = counts.get_mut(&self.name)
        {
            *c = c.saturating_sub(1);
        }
    }
}

/// Everything one chain walk needs. A struct rather than eight positional
/// arguments, because the streaming path builds it on a different thread and the
/// order of five `&str`s is not something a reader should have to hold.
pub struct ChainCtx<'a> {
    pub legs: &'a [&'a str],
    pub prompt: &'a str,
    pub last_user: &'a str,
    pub raw: &'a serde_json::Value,
    pub default_to: Duration,
    pub reqid: &'a str,
    /// §6.3: when present, backends this conversation has already overflowed are
    /// skipped, and a fresh overflow is recorded against it.
    pub session: Option<&'a SessionKey>,
    /// §8: the chain's acceptance gate, if it declared one. `NonEmptyGate` runs
    /// unconditionally either way.
    pub gate: Option<&'a GateCfg>,
}

/// §9.2: the live delta sink plus the flag recording whether it has ever fired.
///
/// They travel together because the commit point is exactly "this sink was
/// called once", and the chain loop needs to see that to know it can no longer
/// fall through.
type DeltaSink<'a> = (&'a mut dyn FnMut(&str) -> bool, &'a mut bool);

/// §9.2: what a streaming chain walk did.
pub struct StreamOutcome {
    pub result: DispatchResult,
    pub winner: String,
    /// True when deltas were relayed live. False means the leg was blocking
    /// (`tmuxlet`/`cli`, permanently) and the caller must emit the frames itself.
    pub streamed: bool,
}

/// Walk the chain, relaying an `api` leg's tokens as they arrive.
///
/// **Streaming and chain fallback are mutually exclusive after the first byte.**
/// Once a `data:` frame has gone out there is no un-sending it, so `on_delta` is
/// only ever called after the upstream has returned a 2xx and a parseable first
/// token — every failure before that point still falls through normally. First
/// token success is the commit point.
pub fn run_streaming(
    state: &State,
    ctx: &ChainCtx,
    on_delta: &mut dyn FnMut(&str) -> bool,
) -> Result<StreamOutcome, Vec<BackendError>> {
    let mut committed = false;
    let (result, winner) = run_inner(state, ctx, Some((on_delta, &mut committed)))?;
    Ok(StreamOutcome {
        result,
        winner,
        streamed: committed,
    })
}

/// Walk the chain with P-9 cooldown, P-6 budget, and U-20 concurrency. Returns
/// the winning result or every leg's error.
pub fn run(state: &State, ctx: &ChainCtx) -> Result<(DispatchResult, String), Vec<BackendError>> {
    run_inner(state, ctx, None)
}

fn run_inner(
    state: &State,
    ctx: &ChainCtx,
    mut on_delta: Option<DeltaSink>,
) -> Result<(DispatchResult, String), Vec<BackendError>> {
    let ChainCtx {
        legs: names,
        prompt,
        last_user,
        raw: raw_value,
        default_to,
        reqid,
        session,
        gate,
    } = *ctx;
    let cooldown_on = state.cfg.server.cooldown;
    // §6.3: an overflow exclusion is per-conversation and permanent for that
    // conversation's life — the prompt only grows, so a leg that could not fit it
    // on turn 5 will not fit it on turn 6.
    let sess = session.and_then(|k| state.sessions.get(k));
    let excluded = sess
        .as_ref()
        .map(|st| st.excluded.clone())
        .unwrap_or_default();
    // §8: once a session has confirmed an escalation, later turns skip both the
    // cheap leg and the judge — there is no point re-paying for a verdict this
    // conversation has already reached.
    let latched = sess.as_ref().and_then(|st| st.latched.clone());
    let latch_at = latched
        .as_ref()
        .and_then(|l| names.iter().position(|n| n == l))
        .map(|i| i + 1)
        .filter(|i| *i < names.len());
    let judge: Option<JudgeGate> = match (gate, latch_at) {
        // A latched session has already escalated past the gated leg, so running
        // the judge again is pure cost.
        (Some(g), None) if g.kind == "judge" => Some(JudgeGate::from_config(g)),
        _ => None,
    };
    let start = Instant::now();
    let budget_deadline = state
        .cfg
        .server
        .chain_budget_secs
        // checked_add: an absurd budget would overflow Instant; treat as unbounded.
        .and_then(|b| start.checked_add(Duration::from_secs(b)));

    // P-9: snapshot cooling status per candidate.
    let cooling: Vec<Option<Duration>> = names
        .iter()
        .map(|&n| {
            if !cooldown_on {
                return None;
            }
            let h = state.health.lock().unwrap();
            h.get(n)
                .and_then(|hh| hh.cooling_until)
                .and_then(|u| u.checked_duration_since(start))
        })
        .collect();

    let mut errors: Vec<BackendError> = Vec::new();
    for (i, &name) in names.iter().enumerate() {
        // §8: skip everything up to and including the leg this session escalated
        // past. No error is recorded — the leg was not tried, and did not fail.
        if latch_at.is_some_and(|start| i < start) {
            log::debug(&format!(
                "{reqid} skip backend={name}: session latched deeper"
            ));
            continue;
        }
        // P-6: budget exhausted — skip the rest.
        if let Some(dl) = budget_deadline
            && Instant::now() >= dl
        {
            errors.push(BackendError::Backend(
                name.to_string(),
                "skipped: budget".into(),
            ));
            log::debug(&format!("{reqid} skip backend={name}: budget exhausted"));
            continue;
        }
        // P-9: skip a cooling leg only if a non-cooling leg remains ahead.
        if let Some(remaining) = cooling[i] {
            let non_cooling_ahead = cooling[i + 1..].iter().any(|c| c.is_none());
            if non_cooling_ahead {
                let secs = remaining.as_secs();
                errors.push(BackendError::Backend(
                    name.to_string(),
                    format!("skipped: cooling {secs}s"),
                ));
                log::debug(&format!("{reqid} skip backend={name}: cooling {secs}s"));
                continue;
            }
        }
        // §6.3: this conversation already proved the prompt does not fit here.
        if excluded.contains(name) {
            errors.push(BackendError::Backend(
                name.to_string(),
                "skipped: context overflow earlier this session".into(),
            ));
            log::debug(&format!("{reqid} skip backend={name}: session overflow"));
            continue;
        }
        let backend = &state.backends[name];
        // U-20: reserve a concurrency slot (RAII: released on Drop, even on a
        // dispatch panic). At the cap this leg is Busy and the chain advances.
        let _slot = match backend.max_concurrent() {
            Some(max) => match ActiveSlot::try_acquire(&state.active, name, max) {
                Some(slot) => Some(slot),
                None => {
                    errors.push(BackendError::Busy(name.to_string()));
                    continue;
                }
            },
            None => None,
        };
        // P-6: clamp the leg timeout to the remaining budget.
        let leg_to = backend.timeout_override().unwrap_or(default_to);
        let timeout = match budget_deadline {
            Some(dl) => dl.saturating_duration_since(Instant::now()).min(leg_to),
            None => leg_to,
        };
        let leg_start = Instant::now();
        let leg_prompt = match backend.prompt_mode() {
            PromptMode::LastUser => last_user,
            PromptMode::Transcript => prompt,
        };
        // §9.2: stream only when the caller wants it, the backend can do it, and
        // no gate is waiting to reject this leg — a gate needs the whole answer
        // before it can judge, and there is no un-sending a relayed token.
        let can_stream = on_delta.is_some() && backend.supports_streaming() && judge.is_none();
        let result = match (&mut on_delta, can_stream) {
            (Some((sink, committed)), true) => {
                let mut wrapped = |d: &str| {
                    // The commit point (§9.2). Recorded HERE rather than in the
                    // caller's closure so the loop below can see it: once a
                    // `data:` frame has gone out there is no un-sending it.
                    **committed = true;
                    sink(d)
                };
                backend.dispatch_stream(leg_prompt, raw_value, &state.env, timeout, &mut wrapped)
            }
            _ => backend.dispatch(leg_prompt, raw_value, &state.env, timeout),
        };
        let committed = on_delta.as_ref().is_some_and(|(_, c)| **c);
        let elapsed_ms = leg_start.elapsed().as_millis() as u64;
        match result {
            Ok(res) => {
                // §8: a leg that dispatched successfully must also clear the
                // chain's gates, or it is rejected and the chain advances — the
                // same path an empty result already takes. NonEmptyGate is V1's
                // hardcoded check, now just the first gate in the list.
                let gctx = GateCtx {
                    state,
                    backend,
                    backend_name: name,
                    prompt: leg_prompt,
                    session,
                    reqid,
                };
                let mut verdict = NonEmptyGate.check(&res, &gctx);
                // Only ask the judge about an answer that is actually there, and
                // only when a deeper leg exists to escalate to.
                if verdict == Verdict::Accept
                    && let Some(j) = &judge
                    && i + 1 < names.len()
                {
                    verdict = j.check(&res, &gctx);
                }
                if let Verdict::Reject(why) = verdict {
                    let e = BackendError::Backend(name.to_string(), why.clone());
                    record_failure(state, name, &e, elapsed_ms);
                    log::warn(&format!("{reqid} skip backend={name}: {why}"));
                    errors.push(e);
                    continue;
                }
                record_success(state, name, elapsed_ms);
                log::info(&format!(
                    "{reqid} ok backend={name} elapsed_ms={elapsed_ms}"
                ));
                return Ok((res, name.to_string()));
            }
            Err(e) => {
                // §9.2: streaming and chain fallback are mutually exclusive after
                // the first byte. A leg that failed *after* relaying a token has
                // already committed the response; advancing would splice a second
                // backend's answer onto the first one mid-stream.
                if committed {
                    log::error(&format!(
                        "{reqid} backend={name} failed after committing the stream: {e}"
                    ));
                    record_failure(state, name, &e, elapsed_ms);
                    errors.push(e);
                    return Err(errors);
                }
                log::warn(&format!("{reqid} skip backend={name}: {e}"));
                if let (BackendError::Overflow(..), Some(key)) = (&e, session) {
                    state.sessions.update(key, |st| {
                        st.excluded.insert(name.to_string());
                    });
                }
                record_failure(state, name, &e, elapsed_ms);
                errors.push(e);
            }
        }
    }
    Err(errors)
}

pub fn record_success(state: &State, name: &str, latency_ms: u64) {
    state
        .stats
        .record_backend(name, Outcome::Success, latency_ms);
    let mut h = state.health.lock().unwrap();
    let e = h.entry(name.to_string()).or_default();
    e.consecutive_failures = 0;
    e.cooling_until = None;
    e.last_error = None;
    e.last_latency_ms = Some(latency_ms);
}

pub fn record_failure(state: &State, name: &str, err: &BackendError, latency_ms: u64) {
    state
        .stats
        .record_backend(name, outcome_for(err), latency_ms);
    if let BackendError::RateLimited(_, after, _) = err {
        // §7.5 tier 1: reactive, always on, zero config. The upstream's own
        // Retry-After beats our guess; absent one, the configured cooldown beats
        // a constant, because an operator who set cooldown_secs = 600 meant it.
        state.stats.record_rate_limited(
            name,
            Some(after.unwrap_or(Duration::from_secs(state.cfg.server.cooldown_secs))),
        );
        // §7.5 tier 3: the window that just hit the wall IS the limit estimate.
        if state
            .cfg
            .backends
            .get(name)
            .is_some_and(|b| b.quota_learn())
        {
            state.stats.learn_limit(name);
        }
    }
    // §4.3: a context overflow says nothing about the backend's health — the
    // backend is fine, the prompt does not fit. Keep it out of the cooldown
    // bookkeeping entirely, or one long conversation benches a 32k local model
    // for every short request that follows.
    if let BackendError::Overflow(..) = err {
        return record_overflow(state, name, err, latency_ms);
    }
    // A4: mutate health under the lock, then RELEASE it before logging. log::line
    // can block on a stalled stderr pipe; blocking (or, before the log.rs fix,
    // panicking on EPIPE) while holding state.health would stall or poison the
    // mutex and brick every later request.
    let cooldown_log = {
        let mut h = state.health.lock().unwrap();
        let e = h.entry(name.to_string()).or_default();
        e.consecutive_failures = e.consecutive_failures.saturating_add(1);
        e.last_error = Some(err.to_string());
        e.last_latency_ms = Some(latency_ms);
        if state.cfg.server.cooldown {
            let cd = cooldown_for(err, e.consecutive_failures, state.cfg.server.cooldown_secs);
            // checked_add: an absurd cooldown_secs would overflow Instant.
            e.cooling_until = Instant::now().checked_add(cd);
            Some((cd.as_secs(), err.class()))
        } else {
            None
        }
    };
    if let Some((secs, class)) = cooldown_log {
        log::info(&format!("cooldown {name} for {secs}s ({class})"));
    }
}

/// §4.3: record a context overflow without letting it read as ill-health.
///
/// `last_error`/`last_latency_ms` still move, so `/v1/backends` can explain why a
/// leg was skipped. `consecutive_failures` is neither incremented **nor reset**
/// (plan O4): an overflow is no more evidence of health than of ill-health, and
/// zeroing the counter would let a chatty client launder a genuinely failing
/// backend back into rotation. No cooldown is applied.
fn record_overflow(state: &State, name: &str, err: &BackendError, latency_ms: u64) {
    let mut h = state.health.lock().unwrap();
    let e = h.entry(name.to_string()).or_default();
    e.last_error = Some(err.to_string());
    e.last_latency_ms = Some(latency_ms);
}

/// §5.3: the bounded `outcome` label. Retryable means "the same request against
/// the same backend could plausibly work in a minute"; everything else is
/// `other_error`, and an overflow is neither.
pub fn outcome_for(err: &BackendError) -> Outcome {
    match err {
        BackendError::Overflow(..) => Outcome::Overflow,
        BackendError::Timeout(_) | BackendError::Spawn(_, _) | BackendError::Busy(_) => {
            Outcome::RetryableError
        }
        BackendError::RateLimited(..) => Outcome::RetryableError,
        BackendError::Http(_, 500..=599, _) => Outcome::RetryableError,
        _ => Outcome::OtherError,
    }
}

/// P-9: cooldown scaled by what a wasted retry costs.
pub fn cooldown_for(err: &BackendError, failures: u32, cooldown_secs: u64) -> Duration {
    match err {
        // A retry burns the whole timeout — back off exponentially 30 -> 300s.
        BackendError::Timeout(_) => {
            let shift = failures.saturating_sub(1).min(4);
            Duration::from_secs((30u64 << shift).min(300))
        }
        // Connect-refused fails in milliseconds; the "just restarted Ollama"
        // case must recover fast.
        BackendError::Spawn(_, msg) if msg.to_ascii_lowercase().contains("refused") => {
            Duration::from_secs(5)
        }
        // P-9: honor the upstream's own Retry-After on a 429. It knows when its
        // window resets and we do not; guessing cooldown_secs either benches a
        // recovered backend or hammers one that is still refusing.
        BackendError::RateLimited(_, Some(after), _) => *after,
        // Other HTTP, and a 429 with no usable header: base cooldown.
        _ => Duration::from_secs(cooldown_secs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::State;

    #[test]
    fn cooldown_scales_with_error_class() {
        assert_eq!(
            cooldown_for(&BackendError::Timeout("t".into()), 1, 60).as_secs(),
            30
        );
        assert_eq!(
            cooldown_for(&BackendError::Timeout("t".into()), 3, 60).as_secs(),
            120
        );
        assert_eq!(
            cooldown_for(&BackendError::Timeout("t".into()), 9, 60).as_secs(),
            300
        );
        assert_eq!(
            cooldown_for(
                &BackendError::Spawn("t".into(), "Connection refused".into()),
                1,
                60
            )
            .as_secs(),
            5
        );
        // P-9: a 429 with no usable Retry-After falls back to the base cooldown.
        assert_eq!(
            cooldown_for(
                &BackendError::RateLimited("t".into(), None, String::new()),
                1,
                60
            )
            .as_secs(),
            60
        );
        // ...and one that carries the header honors it instead of guessing. §12's
        // forced-429 drill turns on exactly this: the leg returns to service when
        // Retry-After elapses, not cooldown_secs later.
        assert_eq!(
            cooldown_for(
                &BackendError::RateLimited(
                    "t".into(),
                    Some(Duration::from_secs(120)),
                    String::new()
                ),
                1,
                60
            )
            .as_secs(),
            120
        );
    }

    #[test]
    fn active_slot_caps_and_frees() {
        // U-20: the guard blocks a second acquire at cap and frees on Drop.
        let active = Mutex::new(HashMap::new());
        let s1 = ActiveSlot::try_acquire(&active, "b", 1);
        assert!(s1.is_some(), "first acquire under cap succeeds");
        assert_eq!(*active.lock().unwrap().get("b").unwrap(), 1);
        assert!(
            ActiveSlot::try_acquire(&active, "b", 1).is_none(),
            "second acquire at cap is refused"
        );
        drop(s1);
        assert_eq!(*active.lock().unwrap().get("b").unwrap(), 0);
        assert!(
            ActiveSlot::try_acquire(&active, "b", 1).is_some(),
            "slot is reusable after Drop"
        );
    }

    #[test]
    fn active_slot_releases_on_unwind() {
        // A2: a panic between acquire and release must not leak the slot (the
        // server survives dispatch panics via F-3 catch_unwind).
        let active = Mutex::new(HashMap::new());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _slot = ActiveSlot::try_acquire(&active, "b", 1).unwrap();
            panic!("boom");
        }));
        assert!(r.is_err());
        assert_eq!(
            *active.lock().unwrap().get("b").unwrap(),
            0,
            "slot must be freed after an unwind"
        );
    }

    #[test]
    fn overflow_does_not_cool_or_count_against_health() {
        // T14: an overflow must move neither consecutive_failures nor
        // cooling_until — and the same call with a Timeout must move both, so a
        // short-circuit that swallowed every error class would fail here.
        const C: &str = r#"
[server]
listen = "127.0.0.1:3456"
default_chain = "echo"
env_source = "process"
cooldown = true

[backends.echo]
type = "cli"
bin = "/bin/echo"
"#;
        let state = State::for_test(crate::config::parse(C).unwrap());
        let overflow = BackendError::Overflow("echo".into(), "maximum context length".into());
        record_failure(&state, "echo", &overflow, 12);
        {
            let h = state.health.lock().unwrap();
            let e = h.get("echo").expect("overflow still records last_error");
            assert_eq!(e.consecutive_failures, 0, "overflow is not a health signal");
            assert!(e.cooling_until.is_none(), "overflow must not cool a leg");
            assert_eq!(e.last_latency_ms, Some(12));
            assert!(
                e.last_error
                    .as_deref()
                    .unwrap()
                    .contains("context overflow"),
                "/v1/backends must still explain the skip: {:?}",
                e.last_error
            );
        }
        record_failure(&state, "echo", &BackendError::Timeout("echo".into()), 34);
        let h = state.health.lock().unwrap();
        let e = h.get("echo").unwrap();
        assert_eq!(e.consecutive_failures, 1, "a real failure still counts");
        assert!(e.cooling_until.is_some(), "a real failure still cools");
    }

    #[test]
    fn outcome_labels_stay_bounded_and_meaningful() {
        // §5.3: the /metrics label set is fixed at four values by construction.
        assert_eq!(
            outcome_for(&BackendError::Overflow("a".into(), String::new())),
            Outcome::Overflow
        );
        assert_eq!(
            outcome_for(&BackendError::Timeout("a".into())),
            Outcome::RetryableError
        );
        assert_eq!(
            outcome_for(&BackendError::RateLimited("a".into(), None, String::new())),
            Outcome::RetryableError
        );
        assert_eq!(
            outcome_for(&BackendError::Http("a".into(), 503, String::new())),
            Outcome::RetryableError
        );
        // A 400 is the client's problem; retrying it changes nothing.
        assert_eq!(
            outcome_for(&BackendError::Http("a".into(), 400, String::new())),
            Outcome::OtherError
        );
        assert_eq!(
            outcome_for(&BackendError::Parse("a".into(), String::new())),
            Outcome::OtherError
        );
    }
}

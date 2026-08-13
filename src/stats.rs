//! §5.2 `/v1/stats` and §5.3 `/metrics`: traffic counters for the routing layer.
//!
//! Self-contained on purpose — no `State`, no `Config`, no `Backend`. Every entry
//! point takes primitives and returns a `serde_json::Value` or a `String`, so the
//! counters can be built, tested, and reasoned about without the server around
//! them.
//!
//! **One lock, one struct.** Everything lives under a single `Mutex<Inner>`. The
//! codebase is disciplined about never holding two locks at once (the A4 comment
//! in `http.rs::record_failure`); with exactly one lock that discipline holds by
//! construction. Nothing that can block — no logging, no I/O, no allocation-free
//! guarantees needed — happens while it is held.
//!
//! **Nothing here changes behavior.** Per §5.2 the whole quota block is pure
//! observation in Phase 1: `limit_hint` and `pressure` read `null` until a limit
//! is configured or learned, so an unconfigured server reports exactly the
//! not-rationing posture it has today. Phase 3 (§7.5) is what starts consuming it.

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// §5.2: "a fixed 512-sample reservoir per backend" — bounded memory, no
/// histogram crate.
const RESERVOIR: usize = 512;

/// The window `calls_in_window` counts over. Phase 1 has no config for it; Phase
/// 3 (§7.5) makes it per-backend configurable as `quota_window_secs`. Rolled
/// lazily on the next read or write — no background thread.
const QUOTA_WINDOW: Duration = Duration::from_secs(3600);

/// Last-resort exhaustion for a 429 whose caller supplied no window at all.
/// §7.5 is "from `Retry-After` when present, else `cooldown_secs`", and every
/// in-tree caller passes the configured `cooldown_secs`, so reaching this
/// constant means a new call site forgot to. Matches `config.rs`'s
/// `default_cooldown_secs` so the mistake is at worst invisible, not wrong.
const DEFAULT_EXHAUSTION: Duration = Duration::from_secs(60);

/// What a single backend dispatch did. A closed set by construction, which is
/// what keeps the §5.3 `outcome=` label bounded: never derive a label from a
/// request or a user value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    Success,
    RetryableError,
    OtherError,
    /// §4.3: the prompt did not fit. Counted apart from `errors` for the same
    /// reason `record_failure` keeps it out of the cooldown bookkeeping — an
    /// overflow says nothing about the backend's health.
    Overflow,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::RetryableError => "retryable_error",
            Outcome::OtherError => "other_error",
            Outcome::Overflow => "overflow",
        }
    }
}

/// Every outcome, so `/metrics` emits a zero series rather than a missing one —
/// `rate()` over a series that only appears on the first error is a broken alert.
const OUTCOMES: [Outcome; 4] = [
    Outcome::Success,
    Outcome::RetryableError,
    Outcome::OtherError,
    Outcome::Overflow,
];

/// Round-robin latency ring. Once full the oldest sample is overwritten, so
/// `p50`/`p95` describe the **most recent 512 dispatches**, not all history —
/// deliberately a recent-window statistic, since the question a router asks is
/// "is this backend slow *now*".
///
/// `max` is tracked separately as an all-time watermark instead of being read off
/// the retained samples: one `u64` is cheaper than a scan, and a 92s outlier that
/// aged out of the ring is exactly the number an operator still wants to see.
#[derive(Default)]
struct Reservoir {
    samples: Vec<u64>,
    next: usize,
    max_ms: u64,
}

impl Reservoir {
    fn record(&mut self, ms: u64) {
        if self.samples.len() < RESERVOIR {
            self.samples.push(ms);
        } else {
            self.samples[self.next] = ms;
            self.next = (self.next + 1) % RESERVOIR;
        }
        self.max_ms = self.max_ms.max(ms);
    }

    /// `null` until a sample lands — a backend nobody dispatched to has no
    /// latency, and reporting `0` would read as "instant".
    fn to_json(&self) -> Value {
        if self.samples.is_empty() {
            return Value::Null;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        json!({
            "p50": percentile(&sorted, 0.50),
            "p95": percentile(&sorted, 0.95),
            "max": self.max_ms,
        })
    }

    fn quantile(&self, q: f64) -> Option<u64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        Some(percentile(&sorted, q))
    }
}

/// Nearest-rank percentile: the smallest retained sample at or above `q`. No
/// interpolation — with integer milliseconds it would invent values that were
/// never measured.
fn percentile(sorted: &[u64], q: f64) -> u64 {
    let n = sorted.len();
    let rank = (n as f64 * q).ceil() as usize;
    sorted[rank.clamp(1, n) - 1]
}

/// Per-backend traffic. The quota fields mirror §7.5's `QuotaState`; here they
/// are counted and nothing more.
struct BackendStat {
    /// §7.5: the window `calls_in_window` counts over. Per-backend, because a
    /// subscription's window and an API key's are not the same length. Defaults
    /// to `QUOTA_WINDOW` until `set_quota_window` says otherwise.
    window: Duration,
    success: u64,
    retryable: u64,
    other: u64,
    overflow: u64,
    latency: Reservoir,
    rate_limited: u64,
    window_start: Instant,
    calls_in_window: u32,
    limit_hint: Option<u32>,
    exhausted_until: Option<Instant>,
    /// When the current exhaustion window opened, so its duration can be banked
    /// into `exhausted_secs_total` once it closes.
    exhausted_since: Option<Instant>,
    exhausted_secs_total: u64,
}

impl BackendStat {
    fn new(now: Instant) -> Self {
        BackendStat {
            window: QUOTA_WINDOW,
            success: 0,
            retryable: 0,
            other: 0,
            overflow: 0,
            latency: Reservoir::default(),
            rate_limited: 0,
            window_start: now,
            calls_in_window: 0,
            limit_hint: None,
            exhausted_until: None,
            exhausted_since: None,
            exhausted_secs_total: 0,
        }
    }

    /// Roll the quota window and bank a finished exhaustion window. Called on
    /// every read and write path, which is what makes both lazy: no timer thread,
    /// and a backend nobody touches costs nothing.
    fn touch(&mut self, now: Instant) {
        // `self.window`, NOT the module const: a backend configured
        // `quota_window_secs = 60` must roll every minute. Reading the const here
        // made the per-backend setting a silent no-op, leaving pressure inflated
        // for the rest of the hour after the provider's real window had reset —
        // so rungs 2 and 4 both misfired for anyone whose window is not 60 min.
        if now.saturating_duration_since(self.window_start) >= self.window {
            self.window_start = now;
            self.calls_in_window = 0;
        }
        if let Some(until) = self.exhausted_until
            && until <= now
        {
            let since = self.exhausted_since.unwrap_or(until);
            self.exhausted_secs_total = self
                .exhausted_secs_total
                .saturating_add(until.saturating_duration_since(since).as_secs());
            self.exhausted_until = None;
            self.exhausted_since = None;
        }
    }

    fn requests(&self) -> u64 {
        self.success
            .saturating_add(self.retryable)
            .saturating_add(self.other)
            .saturating_add(self.overflow)
    }

    /// §5.2 `errors`. Overflow is excluded — it has its own counter and, per §4.3,
    /// is not evidence of ill-health.
    fn errors(&self) -> u64 {
        self.retryable.saturating_add(self.other)
    }

    fn count(&self, outcome: Outcome) -> u64 {
        match outcome {
            Outcome::Success => self.success,
            Outcome::RetryableError => self.retryable,
            Outcome::OtherError => self.other,
            Outcome::Overflow => self.overflow,
        }
    }

    /// Banked seconds **plus the still-open remainder**, so a backend that is
    /// exhausted right now does not report 0.
    fn exhausted_secs(&self, now: Instant) -> u64 {
        let open = match (self.exhausted_until, self.exhausted_since) {
            (Some(until), Some(since)) if until > now => now.saturating_duration_since(since),
            _ => Duration::ZERO,
        };
        self.exhausted_secs_total.saturating_add(open.as_secs())
    }

    /// §7.5 pressure. `None` — not `0.0` — when the limit is unknown and the
    /// backend is not exhausted: the spec's `0.0` is the *routing* value, but
    /// `/v1/stats` has to distinguish "no quota pressure" from "we have no idea",
    /// and rendering `null` is the honest way to say the second one.
    ///
    /// Assumes `touch` already ran, so a lapsed `exhausted_until` is cleared.
    fn pressure(&self, now: Instant) -> Option<f64> {
        if self.exhausted_until.is_some_and(|t| t > now) {
            return Some(1.0);
        }
        match self.limit_hint {
            // limit 0 is a nonsense config, not infinite pressure — and dividing
            // by it is worse than reporting "unknown".
            Some(limit) if limit > 0 => {
                Some((f64::from(self.calls_in_window) / f64::from(limit)).clamp(0.0, 1.0))
            }
            _ => None,
        }
    }
}

struct Inner {
    started: Instant,
    requests_total: u64,
    requests_ok: u64,
    /// Only sources actually seen (§5.2). A `&'static str` key is what bounds the
    /// §5.3 `decision=` label set.
    decisions: BTreeMap<&'static str, u64>,
    backends: BTreeMap<String, BackendStat>,
    classifier_calls: u64,
    classifier_fail_open: u64,
    classifier_total_ms: u64,
}

impl Inner {
    fn backend(&mut self, name: &str, now: Instant) -> &mut BackendStat {
        let stat = self
            .backends
            .entry(name.to_string())
            .or_insert_with(|| BackendStat::new(now));
        stat.touch(now);
        stat
    }

    fn touch_all(&mut self, now: Instant) {
        for stat in self.backends.values_mut() {
            stat.touch(now);
        }
    }
}

pub struct Stats {
    inner: Mutex<Inner>,
}

impl Default for Stats {
    fn default() -> Self {
        Stats::new()
    }
}

impl Stats {
    pub fn new() -> Stats {
        Stats {
            inner: Mutex::new(Inner {
                started: Instant::now(),
                requests_total: 0,
                requests_ok: 0,
                decisions: BTreeMap::new(),
                backends: BTreeMap::new(),
                classifier_calls: 0,
                classifier_fail_open: 0,
                classifier_total_ms: 0,
            }),
        }
    }

    /// Counters are advisory: a poisoned stats mutex must never brick the request
    /// path the way `.unwrap()` would. Nothing under this lock can panic anyway
    /// (saturating arithmetic, no logging), so recovering the guard cannot observe
    /// a half-applied update in practice — and an inaccurate counter beats a 500.
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One chat request finished. `ok` = a leg produced a served answer.
    pub fn record_request(&self, ok: bool) {
        let mut inner = self.inner();
        inner.requests_total = inner.requests_total.saturating_add(1);
        if ok {
            inner.requests_ok = inner.requests_ok.saturating_add(1);
        }
    }

    /// Provenance of the routing decision — "static", "default", "classifier",
    /// "fall_open", and whatever §3 adds later.
    pub fn record_decision(&self, source: &'static str) {
        let mut inner = self.inner();
        *inner.decisions.entry(source).or_insert(0) += 1;
    }

    /// One dispatch against one backend. Latency is recorded for **every**
    /// outcome, not just success: a timeout's full wait is precisely the cost a
    /// router needs to see, and hiding it would make a dying backend look fast.
    pub fn record_backend(&self, name: &str, outcome: Outcome, latency_ms: u64) {
        let now = Instant::now();
        let mut inner = self.inner();
        let stat = inner.backend(name, now);
        match outcome {
            Outcome::Success => stat.success = stat.success.saturating_add(1),
            Outcome::RetryableError => stat.retryable = stat.retryable.saturating_add(1),
            Outcome::OtherError => stat.other = stat.other.saturating_add(1),
            Outcome::Overflow => stat.overflow = stat.overflow.saturating_add(1),
        }
        stat.latency.record(latency_ms);
        // Every dispatch spends quota, including the ones that fail — the upstream
        // counted it too.
        stat.calls_in_window = stat.calls_in_window.saturating_add(1);
    }

    /// A 429 from `name`. Sets `exhausted_until` from `Retry-After` when the
    /// upstream sent one, else `DEFAULT_EXHAUSTION` (§7.5, reactive tier).
    ///
    /// A second 429 while already exhausted **extends but never shortens** the
    /// window: we already know the wall is there until `T`, and a smaller hint is
    /// not evidence that it moved closer.
    pub fn record_rate_limited(&self, name: &str, retry_after: Option<Duration>) {
        let now = Instant::now();
        let mut inner = self.inner();
        let stat = inner.backend(name, now);
        stat.rate_limited = stat.rate_limited.saturating_add(1);
        let until = now
            .checked_add(retry_after.unwrap_or(DEFAULT_EXHAUSTION))
            // An absurd Retry-After would overflow Instant; same guard as the
            // checked_add in http.rs::record_failure.
            .unwrap_or(now);
        stat.exhausted_until = Some(match stat.exhausted_until {
            Some(prev) if prev > until => prev,
            _ => until,
        });
        stat.exhausted_since.get_or_insert(now);
    }

    /// One classifier round-trip. `fell_open` = the reply was unusable.
    pub fn record_classifier(&self, ms: u64, fell_open: bool) {
        let mut inner = self.inner();
        inner.classifier_calls = inner.classifier_calls.saturating_add(1);
        inner.classifier_total_ms = inner.classifier_total_ms.saturating_add(ms);
        if fell_open {
            inner.classifier_fail_open = inner.classifier_fail_open.saturating_add(1);
        }
    }

    /// §7.5 pressure for one backend. `None` when the limit is unknown and the
    /// backend is not currently exhausted.
    ///
    /// Read by the §7.7 cascade: rung 2 skips leg 0 at 1.0, rung 4 takes the fast
    /// path below `reserve_threshold`.
    pub fn pressure(&self, name: &str) -> Option<f64> {
        let now = Instant::now();
        let mut inner = self.inner();
        let stat = inner.backends.get_mut(name)?;
        stat.touch(now);
        stat.pressure(now)
    }

    /// §7.5 tier 2: the operator told us the limit. Called once at startup from
    /// `quota_calls`.
    pub fn set_limit_hint(&self, name: &str, limit: Option<u32>) {
        let now = Instant::now();
        let mut inner = self.inner();
        inner.backend(name, now).limit_hint = limit;
    }

    /// §7.5: override the per-backend quota window. Called once at startup from
    /// `quota_window_secs`; a zero or absent value keeps the default.
    pub fn set_quota_window(&self, name: &str, secs: u64) {
        if secs == 0 {
            return;
        }
        let now = Instant::now();
        let mut inner = self.inner();
        inner.backend(name, now).window = Duration::from_secs(secs);
    }

    /// §7.5 tier 3: learn the limit from where a 429 landed.
    ///
    /// Costs one failed leg per window to discover, and is the only option that
    /// works when the provider's limit is opaque or varies with load. Records the
    /// **current** `calls_in_window` — the count that just proved to be too many —
    /// and only ever lowers an existing hint, so one anomalously high window
    /// cannot inflate the estimate permanently.
    pub fn learn_limit(&self, name: &str) {
        let now = Instant::now();
        let mut inner = self.inner();
        let stat = inner.backend(name, now);
        let observed = stat.calls_in_window.max(1);
        stat.limit_hint = Some(match stat.limit_hint {
            Some(prev) => prev.min(observed),
            None => observed,
        });
    }

    /// `POST /v1/stats/reset` — for benchmarking runs (§5.2).
    ///
    /// Zeroes every counter and restarts the uptime and quota windows. It does
    /// **not** discard `limit_hint` or an open `exhausted_until`: those are not
    /// counters, they are live facts about the upstream. Forgetting a 429 the
    /// provider is still enforcing would make the next report lie, and dropping a
    /// configured limit would silently disable Phase 3 rationing.
    pub fn reset(&self) {
        let now = Instant::now();
        let mut inner = self.inner();
        inner.started = now;
        inner.requests_total = 0;
        inner.requests_ok = 0;
        inner.decisions.clear();
        inner.classifier_calls = 0;
        inner.classifier_fail_open = 0;
        inner.classifier_total_ms = 0;
        for stat in inner.backends.values_mut() {
            let carried = (stat.limit_hint, stat.exhausted_until);
            let mut fresh = BackendStat::new(now);
            fresh.limit_hint = carried.0;
            fresh.exhausted_until = carried.1.filter(|t| *t > now);
            // The open window's banked seconds are gone with the counter, so it
            // restarts here rather than back-dating into the reset run.
            fresh.exhausted_since = fresh.exhausted_until.map(|_| now);
            *stat = fresh;
        }
    }

    /// §5.2 body. `backends` maps every configured backend to its cost class, so
    /// the report covers backends that have not been dispatched to yet; a backend
    /// with recorded traffic but no entry there (renamed or removed from config)
    /// still appears, with `"cost": "unknown"` — dropping measured traffic on the
    /// floor is worse than an odd-looking cost.
    pub fn to_json(&self, backends: &BTreeMap<String, &'static str>) -> Value {
        let now = Instant::now();
        let mut inner = self.inner();
        inner.touch_all(now);
        let entries: serde_json::Map<String, Value> = names(backends, &inner)
            .into_iter()
            .map(|name| {
                let cost = backends.get(name).copied().unwrap_or("unknown");
                let stat = inner.backends.get(name);
                let quota = match stat {
                    Some(s) => json!({
                        "rate_limited": s.rate_limited,
                        "calls_in_window": s.calls_in_window,
                        "limit_hint": s.limit_hint,
                        "pressure": s.pressure(now),
                        "exhausted_secs_total": s.exhausted_secs(now),
                    }),
                    None => json!({
                        "rate_limited": 0,
                        "calls_in_window": 0,
                        "limit_hint": Value::Null,
                        "pressure": Value::Null,
                        "exhausted_secs_total": 0,
                    }),
                };
                let body = json!({
                    "requests": stat.map_or(0, BackendStat::requests),
                    "errors": stat.map_or(0, BackendStat::errors),
                    "overflow": stat.map_or(0, |s| s.overflow),
                    "latency_ms": stat.map_or(Value::Null, |s| s.latency.to_json()),
                    "cost": cost,
                    "quota": quota,
                });
                (name.to_string(), body)
            })
            .collect();
        json!({
            "uptime_secs": now.saturating_duration_since(inner.started).as_secs(),
            "requests": {
                "total": inner.requests_total,
                "ok": inner.requests_ok,
                "failed": inner.requests_total.saturating_sub(inner.requests_ok),
            },
            "decisions": inner.decisions,
            "backends": Value::Object(entries),
            "classifier": {
                "calls": inner.classifier_calls,
                "fail_open": inner.classifier_fail_open,
                // null, not 0, before the first call: a mean of nothing is not zero.
                "mean_ms": match inner.classifier_calls {
                    0 => Value::Null,
                    n => json!(inner.classifier_total_ms / n),
                },
            },
        })
    }

    /// §5.3 Prometheus text exposition format 0.0.4, hand-emitted. No registry, no
    /// crate — the grammar is ~15 lines.
    ///
    /// **Cardinality discipline (§5.3, copied from Switchyard's metrics reference
    /// wholesale, which is the point): the only labels are `backend`, `decision`,
    /// `outcome`, and `quantile`. No per-request or per-user value ever becomes a
    /// label.** `decision` and `outcome` are `&'static str` at the type level;
    /// `backend` is bounded by the config file. Anything unbounded belongs in a
    /// log line, not here.
    /// `known_decisions` is the closed provenance label set (`DecisionSource::ALL`
    /// rendered to strings). Every one is emitted, at 0 if never seen, so no
    /// series is born mid-life — see the note on `DecisionSource::ALL`.
    pub fn to_prometheus(
        &self,
        backends: &BTreeMap<String, &'static str>,
        known_decisions: &[&'static str],
    ) -> String {
        let now = Instant::now();
        let mut inner = self.inner();
        inner.touch_all(now);
        let mut out = String::new();

        family(
            &mut out,
            "tmuxlet_uptime_seconds",
            "Seconds since startup or the last /v1/stats/reset.",
            "gauge",
        );
        out.push_str(&format!(
            "tmuxlet_uptime_seconds {}\n",
            now.saturating_duration_since(inner.started).as_secs()
        ));

        family(
            &mut out,
            "tmuxlet_requests_total",
            "Chat requests by whether any leg produced a served answer.",
            "counter",
        );
        out.push_str(&format!(
            "tmuxlet_requests_total{{outcome=\"ok\"}} {}\n",
            inner.requests_ok
        ));
        out.push_str(&format!(
            "tmuxlet_requests_total{{outcome=\"failed\"}} {}\n",
            inner.requests_total.saturating_sub(inner.requests_ok)
        ));

        family(
            &mut out,
            "tmuxlet_decisions_total",
            "Routing decisions by provenance.",
            "counter",
        );
        let mut sources: Vec<&str> = known_decisions.to_vec();
        for source in inner.decisions.keys() {
            if !sources.contains(source) {
                sources.push(source);
            }
        }
        sources.sort_unstable();
        for source in sources {
            let count = inner.decisions.get(source).copied().unwrap_or(0);
            out.push_str(&format!(
                "tmuxlet_decisions_total{{decision=\"{}\"}} {count}\n",
                escape(source)
            ));
        }

        let names = names(backends, &inner);

        family(
            &mut out,
            "tmuxlet_backend_requests_total",
            "Backend dispatches by outcome.",
            "counter",
        );
        for name in &names {
            let label = escape(name);
            for outcome in OUTCOMES {
                let n = inner.backends.get(*name).map_or(0, |s| s.count(outcome));
                out.push_str(&format!(
                    "tmuxlet_backend_requests_total{{backend=\"{label}\",outcome=\"{}\"}} {n}\n",
                    outcome.label()
                ));
            }
        }

        // A gauge, not a summary: these are quantiles over a 512-sample recent
        // window, and claiming `summary` would tell Prometheus it can aggregate
        // them across instances, which is false.
        family(
            &mut out,
            "tmuxlet_backend_latency_ms",
            "Dispatch latency quantiles over the retained 512-sample window.",
            "gauge",
        );
        for name in &names {
            let label = escape(name);
            let Some(stat) = inner.backends.get(*name) else {
                continue;
            };
            for (q, tag) in [(0.50, "0.5"), (0.95, "0.95")] {
                if let Some(v) = stat.latency.quantile(q) {
                    out.push_str(&format!(
                        "tmuxlet_backend_latency_ms{{backend=\"{label}\",quantile=\"{tag}\"}} {v}\n"
                    ));
                }
            }
        }

        family(
            &mut out,
            "tmuxlet_backend_rate_limited_total",
            "HTTP 429 responses received from a backend.",
            "counter",
        );
        for name in &names {
            let n = inner.backends.get(*name).map_or(0, |s| s.rate_limited);
            out.push_str(&format!(
                "tmuxlet_backend_rate_limited_total{{backend=\"{}\"}} {n}\n",
                escape(name)
            ));
        }

        family(
            &mut out,
            "tmuxlet_backend_exhausted_seconds_total",
            "Cumulative seconds a backend spent rate-limit exhausted.",
            "counter",
        );
        for name in &names {
            let n = inner
                .backends
                .get(*name)
                .map_or(0, |s| s.exhausted_secs(now));
            out.push_str(&format!(
                "tmuxlet_backend_exhausted_seconds_total{{backend=\"{}\"}} {n}\n",
                escape(name)
            ));
        }

        family(
            &mut out,
            "tmuxlet_backend_quota_pressure",
            "Quota pressure 0..1; absent when the limit is unknown.",
            "gauge",
        );
        for name in &names {
            // Omit the series entirely when pressure is None. Emitting 0 would
            // claim "plenty of headroom" for a backend whose limit nobody knows.
            if let Some(p) = inner.backends.get(*name).and_then(|s| s.pressure(now)) {
                out.push_str(&format!(
                    "tmuxlet_backend_quota_pressure{{backend=\"{}\"}} {p:.4}\n",
                    escape(name)
                ));
            }
        }

        family(
            &mut out,
            "tmuxlet_classifier_calls_total",
            "Classifier round-trips.",
            "counter",
        );
        out.push_str(&format!(
            "tmuxlet_classifier_calls_total {}\n",
            inner.classifier_calls
        ));
        family(
            &mut out,
            "tmuxlet_classifier_fail_open_total",
            "Classifier replies that were unusable and fell open.",
            "counter",
        );
        out.push_str(&format!(
            "tmuxlet_classifier_fail_open_total {}\n",
            inner.classifier_fail_open
        ));
        out
    }

    /// Test-only clock seam: shift every stored `Instant` back by `by`, which is
    /// indistinguishable from `by` having elapsed. Beats sleeping in tests that
    /// have to cross a quota window or an exhaustion deadline.
    #[cfg(test)]
    fn rewind(&self, by: Duration) {
        fn back(t: Instant, by: Duration) -> Instant {
            t.checked_sub(by).unwrap_or(t)
        }
        let mut inner = self.inner();
        inner.started = back(inner.started, by);
        for stat in inner.backends.values_mut() {
            stat.window_start = back(stat.window_start, by);
            stat.exhausted_until = stat.exhausted_until.map(|t| back(t, by));
            stat.exhausted_since = stat.exhausted_since.map(|t| back(t, by));
        }
    }
}

/// Every backend worth reporting: configured ∪ observed, sorted.
fn names<'a>(backends: &'a BTreeMap<String, &'static str>, inner: &'a Inner) -> Vec<&'a str> {
    let mut set: BTreeSet<&str> = backends.keys().map(String::as_str).collect();
    set.extend(inner.backends.keys().map(String::as_str));
    set.into_iter().collect()
}

/// Exposition format requires a `# HELP` and a `# TYPE` per metric family. Both
/// are emitted even when the family has no series, so a scraper sees a stable
/// schema from the first scrape.
fn family(out: &mut String, name: &str, help: &str, kind: &str) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
}

/// Label-value escaping per the exposition format: backslash, double quote, and
/// newline. Backend names come from TOML table keys, which can contain all three.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    /// The closed provenance label set the transport passes in production
    /// (`DecisionSource::ALL` rendered to strings). Duplicated here so this
    /// module stays independent of `route::`.
    const TEST_DECISIONS: &[&str] = &["static", "default", "affinity", "classifier", "fall_open"];

    use super::*;

    fn cfg(pairs: &[(&str, &'static str)]) -> BTreeMap<String, &'static str> {
        pairs.iter().map(|(n, c)| ((*n).to_string(), *c)).collect()
    }

    #[test]
    fn percentiles_use_nearest_rank_on_a_known_set() {
        // 1..=100 makes the expected ranks arithmetic rather than a guess: p50 is
        // the 50th value, p95 the 95th. Interpolation would report 50.5 / 95.5 —
        // values never measured — which is why nearest-rank is the rule.
        let s = Stats::new();
        for ms in 1..=100u64 {
            s.record_backend("a", Outcome::Success, ms);
        }
        let v = s.to_json(&cfg(&[("a", "local")]));
        let l = &v["backends"]["a"]["latency_ms"];
        assert_eq!(l["p50"], 50);
        assert_eq!(l["p95"], 95);
        assert_eq!(l["max"], 100);
    }

    #[test]
    fn reservoir_stays_bounded_and_keeps_an_all_time_max() {
        // 10_000 records must not grow memory, and the ring must be a *recent*
        // window: after 10k samples the last 512 are 9489..=10000, so p50 sits in
        // that band and nothing from the first 9k survives. The 99_999 spike from
        // early on is aged out of the ring but must still be reported as max.
        let s = Stats::new();
        s.record_backend("a", Outcome::Success, 99_999);
        for ms in 1..=10_000u64 {
            s.record_backend("a", Outcome::Success, ms);
        }
        {
            let inner = s.inner();
            let stat = &inner.backends["a"];
            assert_eq!(stat.latency.samples.len(), RESERVOIR, "ring must be capped");
            assert_eq!(stat.requests(), 10_001);
        }
        let v = s.to_json(&cfg(&[("a", "local")]));
        let l = &v["backends"]["a"]["latency_ms"];
        assert_eq!(l["max"], 99_999, "max is all-time, not window-local");
        let p50 = l["p50"].as_u64().unwrap();
        assert!(
            (9489..=10_000).contains(&p50),
            "p50 must come from the recent window, got {p50}"
        );
    }

    #[test]
    fn latency_is_null_until_a_sample_lands() {
        // A configured-but-never-dispatched backend still appears (operators need
        // to see the zero), but reporting p50 = 0 would read as "instantaneous".
        let s = Stats::new();
        let v = s.to_json(&cfg(&[("a", "subscription")]));
        assert_eq!(v["backends"]["a"]["latency_ms"], Value::Null);
        assert_eq!(v["backends"]["a"]["requests"], 0);
        assert_eq!(v["backends"]["a"]["cost"], "subscription");
    }

    #[test]
    fn decisions_omit_sources_never_seen() {
        // §5.2: only sources actually seen. A zero for "classifier" on a server
        // with no classifier configured is a wrong answer, not a helpful default.
        let s = Stats::new();
        s.record_decision("static");
        s.record_decision("static");
        s.record_decision("fall_open");
        let v = s.to_json(&cfg(&[]));
        assert_eq!(v["decisions"]["static"], 2);
        assert_eq!(v["decisions"]["fall_open"], 1);
        assert!(v["decisions"].get("classifier").is_none());
        assert_eq!(v["decisions"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn pressure_is_none_without_a_limit() {
        // The Phase 1 posture in one assertion: traffic recorded, no limit known,
        // so nothing to ration on and /v1/stats renders null.
        let s = Stats::new();
        s.record_backend("a", Outcome::Success, 10);
        assert_eq!(s.pressure("a"), None);
        assert_eq!(s.pressure("never-seen"), None);
        let v = s.to_json(&cfg(&[("a", "subscription")]));
        assert_eq!(v["backends"]["a"]["pressure"], Value::Null);
        assert_eq!(v["backends"]["a"]["quota"]["limit_hint"], Value::Null);
    }

    #[test]
    fn pressure_is_one_while_exhausted_then_clears() {
        // A 429 is a hard 1.0 regardless of any limit, and it must lapse on its
        // own — a permanently-exhausted backend would be a routing black hole.
        let s = Stats::new();
        s.record_rate_limited("a", Some(Duration::from_secs(30)));
        assert_eq!(s.pressure("a"), Some(1.0));
        s.rewind(Duration::from_secs(31));
        assert_eq!(s.pressure("a"), None, "exhaustion must expire");
    }

    #[test]
    fn pressure_is_the_ratio_when_a_limit_is_known() {
        let s = Stats::new();
        s.set_limit_hint("a", Some(200));
        for _ in 0..50 {
            s.record_backend("a", Outcome::Success, 1);
        }
        assert_eq!(s.pressure("a"), Some(0.25));
        // Clamped: 300 calls against a 200 limit is 1.0, not 1.5.
        for _ in 0..250 {
            s.record_backend("a", Outcome::Success, 1);
        }
        assert_eq!(s.pressure("a"), Some(1.0));
        // A zero limit is a nonsense config, not infinite pressure or a panic.
        s.set_limit_hint("a", Some(0));
        assert_eq!(s.pressure("a"), None);
        s.set_limit_hint("a", None);
        assert_eq!(s.pressure("a"), None);
    }

    #[test]
    fn exhausted_secs_includes_the_open_remainder() {
        // A backend exhausted *right now* must not read as 0 — that is exactly the
        // moment an operator is looking at the number.
        let s = Stats::new();
        s.record_rate_limited("a", Some(Duration::from_secs(60)));
        s.rewind(Duration::from_secs(10));
        let v = s.to_json(&cfg(&[("a", "subscription")]));
        assert_eq!(v["backends"]["a"]["quota"]["exhausted_secs_total"], 10);
        assert_eq!(v["backends"]["a"]["quota"]["pressure"], 1.0);
        // Once the window closes the full 60s is banked, and only once: reading
        // twice must not double-count.
        s.rewind(Duration::from_secs(60));
        let v = s.to_json(&cfg(&[("a", "subscription")]));
        assert_eq!(v["backends"]["a"]["quota"]["exhausted_secs_total"], 60);
        let v = s.to_json(&cfg(&[("a", "subscription")]));
        assert_eq!(v["backends"]["a"]["quota"]["exhausted_secs_total"], 60);
        assert_eq!(v["backends"]["a"]["quota"]["rate_limited"], 1);
    }

    #[test]
    fn a_second_429_extends_but_never_shortens_the_window() {
        let s = Stats::new();
        s.record_rate_limited("a", Some(Duration::from_secs(300)));
        s.record_rate_limited("a", Some(Duration::from_secs(5)));
        s.rewind(Duration::from_secs(10));
        assert_eq!(s.pressure("a"), Some(1.0), "5s must not truncate 300s");
        // Default exhaustion applies when the upstream sent no Retry-After.
        let s = Stats::new();
        s.record_rate_limited("b", None);
        s.rewind(DEFAULT_EXHAUSTION - Duration::from_secs(1));
        assert_eq!(s.pressure("b"), Some(1.0));
        s.rewind(Duration::from_secs(2));
        assert_eq!(s.pressure("b"), None);
    }

    #[test]
    fn a_configured_quota_window_actually_governs_the_rollover() {
        // §7.5: `quota_window_secs` is per-backend because a subscription's window
        // and an API key's are not the same length. This test exists because
        // `touch()` read the module const instead of the configured value, which
        // made the setting a silent no-op — and the sibling test below could not
        // catch it, since it rewinds by exactly QUOTA_WINDOW and passes either
        // way.
        let s = Stats::new();
        s.set_quota_window("a", 60);
        s.set_limit_hint("a", Some(10));
        for _ in 0..5 {
            s.record_backend("a", Outcome::Success, 1);
        }
        assert_eq!(s.pressure("a"), Some(0.5));
        // Past the CONFIGURED minute, nowhere near the default hour.
        s.rewind(Duration::from_secs(61));
        assert_eq!(
            s.pressure("a"),
            Some(0.0),
            "a 60s window must roll at 61s, not at 3601s"
        );
        // A backend with no configured window still uses the default.
        let d = Stats::new();
        d.set_limit_hint("b", Some(10));
        for _ in 0..5 {
            d.record_backend("b", Outcome::Success, 1);
        }
        d.rewind(Duration::from_secs(61));
        assert_eq!(
            d.pressure("b"),
            Some(0.5),
            "an unconfigured backend keeps the default hour"
        );
    }

    #[test]
    fn quota_window_rolls_over_lazily() {
        let s = Stats::new();
        s.set_limit_hint("a", Some(10));
        for _ in 0..5 {
            s.record_backend("a", Outcome::Success, 1);
        }
        assert_eq!(s.pressure("a"), Some(0.5));
        s.rewind(QUOTA_WINDOW + Duration::from_secs(1));
        // Rolled on read: the old window's calls no longer apply pressure, but the
        // lifetime request count is untouched.
        assert_eq!(s.pressure("a"), Some(0.0));
        let v = s.to_json(&cfg(&[("a", "subscription")]));
        assert_eq!(v["backends"]["a"]["quota"]["calls_in_window"], 0);
        assert_eq!(v["backends"]["a"]["requests"], 5);
    }

    #[test]
    fn overflow_counts_apart_from_errors() {
        // §4.3: an overflow is not a health signal. It counts as a request and in
        // its own bucket, never in `errors`.
        let s = Stats::new();
        s.record_backend("a", Outcome::Success, 1);
        s.record_backend("a", Outcome::RetryableError, 2);
        s.record_backend("a", Outcome::OtherError, 3);
        s.record_backend("a", Outcome::Overflow, 4);
        let v = s.to_json(&cfg(&[("a", "local")]));
        assert_eq!(v["backends"]["a"]["requests"], 4);
        assert_eq!(v["backends"]["a"]["errors"], 2);
        assert_eq!(v["backends"]["a"]["overflow"], 1);
    }

    #[test]
    fn reset_zeroes_counters_and_restarts_the_uptime_window() {
        let s = Stats::new();
        s.record_request(true);
        s.record_request(false);
        s.record_decision("static");
        s.record_backend("a", Outcome::Success, 500);
        s.record_classifier(200, true);
        s.set_limit_hint("a", Some(100));
        s.rewind(Duration::from_secs(90));
        assert_eq!(s.to_json(&cfg(&[("a", "subscription")]))["uptime_secs"], 90);

        s.reset();
        let v = s.to_json(&cfg(&[("a", "subscription")]));
        assert_eq!(v["uptime_secs"], 0, "uptime window restarts");
        assert_eq!(v["requests"]["total"], 0);
        assert_eq!(v["requests"]["ok"], 0);
        assert_eq!(v["requests"]["failed"], 0);
        assert_eq!(v["decisions"].as_object().unwrap().len(), 0);
        assert_eq!(v["classifier"]["calls"], 0);
        assert_eq!(v["classifier"]["mean_ms"], Value::Null);
        assert_eq!(v["backends"]["a"]["requests"], 0);
        assert_eq!(v["backends"]["a"]["latency_ms"], Value::Null);
        assert_eq!(v["backends"]["a"]["quota"]["calls_in_window"], 0);
        assert_eq!(v["backends"]["a"]["quota"]["exhausted_secs_total"], 0);
        // The configured limit survives: it is config, not a counter. Dropping it
        // would silently disable Phase 3 rationing after a benchmark run.
        assert_eq!(v["backends"]["a"]["quota"]["limit_hint"], 100);
        assert_eq!(v["backends"]["a"]["quota"]["pressure"], 0.0);
    }

    #[test]
    fn reset_keeps_an_exhaustion_the_upstream_is_still_enforcing() {
        // Zeroing a 429 the provider has not lifted would make the next report
        // lie about a backend that is genuinely unavailable.
        let s = Stats::new();
        s.record_rate_limited("a", Some(Duration::from_secs(300)));
        s.reset();
        assert_eq!(s.pressure("a"), Some(1.0));
        let v = s.to_json(&cfg(&[("a", "subscription")]));
        assert_eq!(
            v["backends"]["a"]["quota"]["rate_limited"], 0,
            "counter zeroed"
        );
        assert_eq!(v["backends"]["a"]["quota"]["exhausted_secs_total"], 0);
    }

    #[test]
    fn json_matches_the_spec_5_2_key_names() {
        // The §5.2 contract verbatim — a renamed key is a broken client, and this
        // is the only place that fact is checked.
        let s = Stats::new();
        s.record_request(true);
        s.record_backend("claude-max", Outcome::Success, 8200);
        s.record_rate_limited("claude-max", Some(Duration::from_secs(60)));
        s.record_classifier(240, false);
        let v = s.to_json(&cfg(&[("claude-max", "subscription")]));

        let top: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(
            top,
            [
                "backends",
                "classifier",
                "decisions",
                "requests",
                "uptime_secs"
            ]
        );
        assert_eq!(
            v["requests"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["failed", "ok", "total"]
        );
        let b = &v["backends"]["claude-max"];
        assert_eq!(
            b.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "cost",
                "errors",
                "latency_ms",
                "overflow",
                "quota",
                "requests"
            ]
        );
        assert_eq!(
            b["latency_ms"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["max", "p50", "p95"]
        );
        assert_eq!(
            b["quota"].as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "calls_in_window",
                "exhausted_secs_total",
                "limit_hint",
                "pressure",
                "rate_limited"
            ]
        );
        assert_eq!(
            v["classifier"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["calls", "fail_open", "mean_ms"]
        );
        assert_eq!(v["classifier"]["mean_ms"], 240);
        assert_eq!(v["requests"]["failed"], 0);
    }

    #[test]
    fn json_reports_traffic_for_a_backend_missing_from_the_config_map() {
        // Renamed or removed from config mid-run: the traffic still happened, so
        // it is reported with an unknown cost rather than silently dropped.
        let s = Stats::new();
        s.record_backend("ghost", Outcome::OtherError, 7);
        let v = s.to_json(&cfg(&[("a", "local")]));
        assert_eq!(v["backends"]["ghost"]["cost"], "unknown");
        assert_eq!(v["backends"]["ghost"]["errors"], 1);
        assert_eq!(v["backends"]["a"]["cost"], "local");
    }

    #[test]
    fn metrics_emit_help_and_type_for_every_series() {
        // Structural, not a golden file: every sample line must be preceded by a
        // HELP and a TYPE for its family. Adding a metric without them fails here.
        let s = Stats::new();
        s.record_request(true);
        s.record_request(false);
        s.record_decision("classifier");
        s.record_backend("a", Outcome::Success, 120);
        s.record_backend("a", Outcome::RetryableError, 9000);
        s.record_rate_limited("a", Some(Duration::from_secs(60)));
        s.record_classifier(240, true);
        let text = s.to_prometheus(
            &cfg(&[("a", "subscription"), ("b", "local")]),
            TEST_DECISIONS,
        );

        let mut helped = BTreeSet::new();
        let mut typed = BTreeSet::new();
        let mut samples = 0;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let (name, help) = rest.split_once(' ').expect("HELP needs text");
                assert!(!help.is_empty());
                assert!(helped.insert(name.to_string()), "duplicate HELP {name}");
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, kind) = rest.split_once(' ').expect("TYPE needs a kind");
                assert!(["counter", "gauge"].contains(&kind), "bad type {kind}");
                assert!(typed.insert(name.to_string()), "duplicate TYPE {name}");
            } else {
                let name = line.split(['{', ' ']).next().unwrap();
                assert!(helped.contains(name), "{name} has no # HELP");
                assert!(typed.contains(name), "{name} has no # TYPE");
                assert!(line.ends_with(|c: char| c.is_ascii_digit()), "{line}");
                samples += 1;
            }
        }
        assert_eq!(helped, typed);
        assert!(samples >= 20, "expected a full scrape, got {samples}");
        // Counters end in _total; the never-dispatched backend still emits zeros
        // so rate() does not see a series appear from nowhere.
        assert!(
            text.contains(
                "tmuxlet_backend_requests_total{backend=\"b\",outcome=\"other_error\"} 0\n"
            )
        );
        assert!(
            text.contains("tmuxlet_backend_requests_total{backend=\"a\",outcome=\"success\"} 1\n")
        );
        assert!(text.contains("tmuxlet_decisions_total{decision=\"classifier\"} 1\n"));
        assert!(text.contains("tmuxlet_requests_total{outcome=\"failed\"} 1\n"));
        assert!(
            text.contains("tmuxlet_backend_latency_ms{backend=\"a\",quantile=\"0.95\"} 9000\n")
        );
        assert!(text.contains("tmuxlet_classifier_fail_open_total 1\n"));
    }

    #[test]
    fn metrics_omit_the_pressure_series_when_it_is_unknown() {
        // Emitting 0 for an unknown limit would claim headroom nobody measured.
        let s = Stats::new();
        s.record_backend("a", Outcome::Success, 5);
        let text = s.to_prometheus(&cfg(&[("a", "subscription")]), TEST_DECISIONS);
        assert!(text.contains("# TYPE tmuxlet_backend_quota_pressure gauge"));
        assert!(
            !text.contains("tmuxlet_backend_quota_pressure{"),
            "no series until a limit is known:\n{text}"
        );
        // Known limit → the series appears; a 429 pins it to 1.
        s.set_limit_hint("a", Some(4));
        let text = s.to_prometheus(&cfg(&[("a", "subscription")]), TEST_DECISIONS);
        assert!(text.contains("tmuxlet_backend_quota_pressure{backend=\"a\"} 0.2500\n"));
        s.record_rate_limited("a", Some(Duration::from_secs(60)));
        let text = s.to_prometheus(&cfg(&[("a", "subscription")]), TEST_DECISIONS);
        assert!(text.contains("tmuxlet_backend_quota_pressure{backend=\"a\"} 1.0000\n"));
    }

    #[test]
    fn label_values_are_escaped() {
        // Backend names are TOML table keys and can hold quotes, backslashes, and
        // newlines; unescaped, any of them corrupts the whole scrape.
        let s = Stats::new();
        s.record_backend("we\"ird\\one\nx", Outcome::Success, 5);
        let text = s.to_prometheus(&cfg(&[]), TEST_DECISIONS);
        assert!(
            text.contains(
                "tmuxlet_backend_requests_total{backend=\"we\\\"ird\\\\one\\nx\",outcome=\"success\"} 1\n"
            ),
            "{text}"
        );
        // The raw newline must not survive into the output as a line break.
        assert!(!text.contains("ird\\one\nx"));
        assert_eq!(escape("plain"), "plain");
    }
}

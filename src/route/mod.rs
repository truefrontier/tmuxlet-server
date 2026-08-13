//! The decision layer.
//!
//! V1 routed by resolving a string once, ahead of dispatch: `resolve(model, cfg)`
//! turned a model name into an ordered list of backend names and `run_chain`
//! walked it. There was nowhere to *put* a routing decision — no type carrying
//! why a chain was chosen, and no provenance on the response.
//!
//! A [`Policy`](policy::Policy) now produces a [`Decision`], which the chain
//! executes and the transport reports. The chain itself is unchanged: policies
//! produce chains, they do not replace them.

pub mod policy;
pub mod resolve;
pub mod scorer;
pub mod session;
pub mod signals;

use crate::config::Config;
use crate::proto::openai::ChatMessage;
use session::SessionKey;

/// Everything a policy is allowed to look at.
///
/// Borrowed throughout — policies never own request state, so constructing one
/// per request costs nothing.
pub struct RequestCtx<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatMessage],
    /// The verbatim request JSON. §7.1's signal extraction reads `tool_calls` and
    /// `role:"tool"` results out of it — fields the typed `ChatMessage` view
    /// deliberately drops.
    pub raw: &'a serde_json::Value,
    pub cfg: &'a Config,
    /// B3/U-8: the request id, so a policy's own log lines correlate with the
    /// closing summary line the transport emits.
    pub reqid: &'a str,
    /// §6: this conversation's identity, when the client gave us one. `None`
    /// means every turn is routed from scratch, which is V1's behavior.
    pub session: Option<&'a SessionKey>,
}

/// The outcome of routing. Produced before dispatch, attached to the response.
pub struct Decision {
    /// Ordered backend names — the chain to walk. Never empty.
    pub legs: Vec<String>,
    /// Why these legs. Drives the rationale header and the stats label.
    pub source: DecisionSource,
    /// Human-readable. Bounded and control-stripped on the way out by
    /// [`Decision::rationale_header`]; store it unbounded here.
    pub rationale: String,
    /// `class/target` for routers; `None` for a direct chain or backend.
    pub label: Option<String>,
    /// §8: the chain these legs came from, so the transport can find its gate.
    /// `None` when the model named a single backend — a one-leg chain has nowhere
    /// to escalate to anyway.
    pub chain: Option<String>,
}

/// The longest `x-tmuxlet-rationale` we will emit. A header is not a log line,
/// and an unbounded one is a denial-of-service surface on any proxy in the path.
const RATIONALE_MAX: usize = 200;

/// The longest component we will echo into `x-tmuxlet-route`. `model` comes
/// straight off the request and is bounded only by the 16 MiB body cap, so
/// without this a client can make us emit a header that its own proxy rejects.
/// Class, target, and backend names come from config and are bounded in
/// practice, but they go through the same cap so the total stays predictable.
const ROUTE_PART_MAX: usize = 64;

fn route_part(s: &str) -> String {
    s.chars().take(ROUTE_PART_MAX).collect()
}

/// Make a string safe to put in an HTTP header value.
///
/// `tiny_http::Header::from_bytes` rejects anything outside printable
/// ISO-8859-1, and the `header()` helper `expect`s on it — so one non-ASCII char
/// panics the worker, which answers a bare bodyless 500. Every value in this
/// module is attacker- or operator-influenced: model, class, target, and backend
/// names come from config, and the rationale quotes them back. Sanitize rather
/// than trust.
pub fn header_safe(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            c if c.is_ascii_graphic() || c == ' ' => c,
            // Newlines and tabs would also be header *injection* if they got
            // through; collapsing them to a space keeps the text readable.
            c if c.is_control() => ' ',
            // A non-ASCII char is dropped to a marker rather than deleted, so
            // truncated-looking output is visibly truncated.
            _ => '.',
        })
        .collect()
}

impl Decision {
    /// `x-tmuxlet-route`: `model[/class/target]/backend`. Always emitted, unlike
    /// V1's router-only header — a plain chain request used to carry no
    /// provenance at all.
    pub fn route_header(&self, model: &str, winner: &str) -> String {
        let (model, winner) = (route_part(model), route_part(winner));
        header_safe(&match &self.label {
            Some(l) => format!("{model}/{}/{winner}", route_part(l)),
            None => format!("{model}/{winner}"),
        })
    }

    /// The same header without a winner, for the streaming path: SSE headers are
    /// written before the chain runs, and naming a leg that might yet fall
    /// through would be a lie. Shares `route_part`, so the client-supplied
    /// `model` is bounded here too — an 80 KB model name otherwise produced an
    /// 80 KB response header that nginx turns into a 502.
    pub fn route_header_prefix(&self, model: &str) -> String {
        let model = route_part(model);
        header_safe(&match &self.label {
            Some(l) => format!("{model}/{}", route_part(l)),
            None => model,
        })
    }

    /// `x-tmuxlet-rationale`, bounded and control-stripped.
    ///
    /// Returns `None` when `redact` is set (S-6, bound non-loopback): the
    /// rationale names backends and can quote upstream error text, which is a
    /// real disclosure surface to a remote client, not a formality.
    pub fn rationale_header(&self, redact: bool) -> Option<String> {
        if redact {
            return None;
        }
        let clean: String = header_safe(&self.rationale)
            .chars()
            .take(RATIONALE_MAX)
            .collect();
        let trimmed = clean.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }
}

/// Provenance. Deliberately mirrors Switchyard's taxonomy so the two are
/// comparable when both are deployed — with `Static`/`Default` added, because our
/// model string can name a chain directly and theirs cannot.
///
/// Every variant is constructed somewhere: CI runs
/// `clippy --all-targets -- -D warnings`, and a variant nothing constructs is a
/// hard failure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecisionSource {
    /// The model named a chain or a backend.
    Static,
    /// Unknown model → `default_chain`.
    Default,
    /// §6.3: reused this session's earlier decision, skipping the classifier
    /// entirely. That skip is the cost win affinity exists for.
    Affinity,
    /// §7.7 rung 2: leg 0 is past its quota wall (429 / `Retry-After`). Skipped,
    /// **not cooled** — the backend is healthy, it is just out of headroom.
    Quota,
    /// §7.7 rung 3: a critical tool error or a compacted context forced the
    /// flat-rate leg, regardless of pressure.
    Override,
    /// §7.7 rung 5: tests passed and work was produced, so the run is settled
    /// enough to shed a leg while rationing.
    Settled,
    /// §7.7 rung 6: the scorer crossed `confidence_threshold` while rationing.
    Dimensions,
    /// The classifier returned a usable class.
    Classifier,
    /// We tried to be smart and could not: the classifier was missing, at its
    /// concurrency cap, errored, or answered ambiguously.
    ///
    /// **Not a synonym for `Default`.** `Default` means the client asked for
    /// something we do not know; `FallOpen` means we tried and failed. Conflating
    /// them is exactly what made V1 hard to debug — both landed in the same
    /// silent `fallback_class`.
    FallOpen,
}

impl DecisionSource {
    /// Every label this build can emit, in a stable order.
    ///
    /// §5.3: a Prometheus counter that springs into existence on its first
    /// increment is a counter whose `rate()` is wrong for the first scrape after
    /// it appears, and whose absence an alert cannot distinguish from zero. The
    /// label set is closed by construction, so every series is emitted from
    /// startup at 0. (`/v1/stats` still omits unseen sources — §5.2 asks for the
    /// observed set, and a JSON report is read by a human, not a range query.)
    pub const ALL: &'static [DecisionSource] = &[
        DecisionSource::Static,
        DecisionSource::Default,
        DecisionSource::Affinity,
        DecisionSource::Quota,
        DecisionSource::Override,
        DecisionSource::Settled,
        DecisionSource::Dimensions,
        DecisionSource::Classifier,
        DecisionSource::FallOpen,
    ];

    /// The stable label used in logs, the `x-tmuxlet-decision` header, and the
    /// `/v1/stats` and `/metrics` keys. `&'static str` on purpose: it keeps the
    /// Prometheus label set bounded by construction.
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionSource::Static => "static",
            DecisionSource::Default => "default",
            DecisionSource::Affinity => "affinity",
            DecisionSource::Quota => "quota",
            DecisionSource::Override => "override",
            DecisionSource::Settled => "settled",
            DecisionSource::Dimensions => "dimensions",
            DecisionSource::Classifier => "classifier",
            DecisionSource::FallOpen => "fall_open",
        }
    }
}

/// A routing failure the client must be told about.
///
/// The spec writes `Policy::decide -> Decision`, with no error path. Three real
/// V1 behaviors need one: `strict_models` answers 404 on an unknown model, an
/// unresolvable router target answers 500, and an unresolvable `default_chain`
/// answers 400. Swallowing those into a fallback chain would be a behavior
/// regression, so `decide` returns a `Result` and the transport renders this.
pub struct RouteError {
    pub status: u16,
    pub message: String,
    pub etype: &'static str,
    pub code: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(label: Option<&str>, rationale: &str) -> Decision {
        Decision {
            legs: vec!["a".into()],
            source: DecisionSource::Static,
            rationale: rationale.to_string(),
            label: label.map(str::to_string),
            chain: None,
        }
    }

    #[test]
    fn route_header_includes_the_model_for_direct_and_router_requests() {
        // §5.1: widened from router-only. A plain chain request used to get no
        // header at all, which made "which backend answered?" unanswerable.
        assert_eq!(
            decision(Some("code/do-code"), "").route_header("do", "agy"),
            "do/code/do-code/agy"
        );
        assert_eq!(
            decision(None, "").route_header("default", "agy"),
            "default/agy"
        );
    }

    #[test]
    fn rationale_is_bounded_and_control_stripped() {
        let d = decision(None, &format!("start\n\ttab{}", "x".repeat(400)));
        let h = d.rationale_header(false).unwrap();
        assert!(h.chars().count() <= RATIONALE_MAX, "unbounded header: {h}");
        assert!(
            !h.contains('\n') && !h.contains('\t'),
            "control chars: {h:?}"
        );
        assert!(h.starts_with("start  tab"), "{h}");
    }

    #[test]
    fn rationale_is_suppressed_when_errors_are_redacted() {
        // S-6: the rationale names backends and can quote upstream text. On a
        // non-loopback bind that is a disclosure surface, so it is dropped whole
        // rather than sanitized.
        assert!(
            decision(None, "ollama refused: secret trace")
                .rationale_header(true)
                .is_none()
        );
    }

    #[test]
    fn an_empty_rationale_emits_no_header() {
        assert!(decision(None, "   ").rationale_header(false).is_none());
    }

    #[test]
    fn an_oversized_client_model_cannot_produce_an_oversized_header() {
        // `model` is client-supplied and bounded only by the 16 MiB body cap. An
        // unbounded response header is rejected by real proxies, so every
        // component is capped and the winner still survives at the tail.
        let d = decision(Some("code/do-code"), "");
        let h = d.route_header(&"m".repeat(10_000), "agy");
        assert!(
            h.chars().count() <= 4 * (ROUTE_PART_MAX + 1),
            "unbounded: {}",
            h.len()
        );
        assert!(
            h.ends_with("/agy"),
            "the winner must survive truncation: {h}"
        );
    }

    #[test]
    fn the_streaming_route_header_is_bounded_too() {
        // The streaming path used to inline its own format!, so the cap added to
        // route_header did not apply to it — an 80 KB model produced an 80 KB SSE
        // response header, written before the chain even ran.
        let d = decision(Some("code/do-code"), "");
        let h = d.route_header_prefix(&"m".repeat(80_000));
        assert!(
            h.chars().count() <= 2 * (ROUTE_PART_MAX + 1),
            "unbounded: {}",
            h.len()
        );
        assert!(h.ends_with("/code/do-code"), "the label must survive: {h}");
        assert_eq!(decision(None, "").route_header_prefix("plain"), "plain");
    }

    #[test]
    fn a_non_ascii_rationale_cannot_panic_the_worker() {
        // tiny_http rejects a non-ISO-8859-1 header value and `header()` expects
        // on it, so an arrow in a rationale used to answer a bare bodyless 500.
        let d = decision(Some("code/tâche"), "classifier chose 'code' → tâche");
        let h = d.rationale_header(false).unwrap();
        assert!(h.is_ascii(), "rationale must be ASCII-safe: {h}");
        assert!(
            d.route_header("dö", "agy").is_ascii(),
            "config-supplied names reach the route header too"
        );
    }

    #[test]
    fn decision_source_labels_are_stable() {
        // These strings are a public contract: they key /v1/stats and become
        // Prometheus label values. Changing one silently breaks a dashboard.
        assert_eq!(DecisionSource::Static.as_str(), "static");
        assert_eq!(DecisionSource::Default.as_str(), "default");
        assert_eq!(DecisionSource::Affinity.as_str(), "affinity");
        assert_eq!(DecisionSource::Quota.as_str(), "quota");
        assert_eq!(DecisionSource::Override.as_str(), "override");
        assert_eq!(DecisionSource::Settled.as_str(), "settled");
        assert_eq!(DecisionSource::Dimensions.as_str(), "dimensions");
        assert_eq!(DecisionSource::Classifier.as_str(), "classifier");
        assert_eq!(DecisionSource::FallOpen.as_str(), "fall_open");
        // ALL must stay exhaustive: a variant missing from it is a Prometheus
        // series that never appears until it fires.
        let labels: Vec<&str> = DecisionSource::ALL.iter().map(|d| d.as_str()).collect();
        for d in [
            DecisionSource::Static,
            DecisionSource::Default,
            DecisionSource::Affinity,
            DecisionSource::Quota,
            DecisionSource::Override,
            DecisionSource::Settled,
            DecisionSource::Dimensions,
            DecisionSource::Classifier,
            DecisionSource::FallOpen,
        ] {
            assert!(labels.contains(&d.as_str()), "{d:?} missing from ALL");
        }
        assert_eq!(labels.len(), 9, "a new variant needs an ALL entry");
    }
}

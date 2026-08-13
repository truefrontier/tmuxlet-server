//! §6: per-conversation state — routing affinity and overflow exclusions.
//!
//! The cost win is affinity: the first turn of a session pays the classifier
//! round-trip, every later turn reuses the decision and skips it entirely. The
//! correctness win is exclusion: a backend that could not fit the prompt on turn
//! 5 will not fit it on turn 6 either, so the chain stops offering it.

use super::DecisionSource;
use crate::proto::openai::ChatMessage;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Headers a coding agent might already be sending, in priority order.
///
/// Recognizing the harness-native ones is the whole point: a client that already
/// tracks a conversation id should not have to learn ours to get affinity.
const SESSION_HEADERS: &[&str] = &[
    "x-tmuxlet-session-id",
    "x-claude-code-session-id", // Claude Code
    "x-session-id",             // generic
    "x-conversation-id",        // Continue
];

/// How a conversation is identified.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum SessionKey {
    /// A client-supplied id. Authoritative, and the only form we trust to be
    /// stable across turns.
    Header(String),
    /// A hash of the opening user message. Opt-in, because it **collides across
    /// independent conversations that open with an identical prompt** — a real
    /// hazard for templated agent harnesses, where every run starts with the same
    /// system-shaped first turn.
    MessageHash(u64),
}

impl SessionKey {
    /// The `x-tmuxlet-session` echo value and the log label. Truncated and
    /// sanitized, because the header form is attacker-supplied.
    pub fn label(&self) -> String {
        match self {
            SessionKey::Header(h) => super::header_safe(&h.chars().take(64).collect::<String>()),
            SessionKey::MessageHash(h) => format!("hash-{h:016x}"),
        }
    }
}

/// Find a session id among the request headers. `headers` is `(name, value)`
/// with names already lowercased by the caller.
pub fn key_from_headers(headers: &[(String, String)]) -> Option<SessionKey> {
    for want in SESSION_HEADERS {
        if let Some((_, v)) = headers.iter().find(|(n, _)| n == want) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(SessionKey::Header(v.to_string()));
            }
        }
    }
    None
}

/// §6.1 fallback: hash the opening user message.
///
/// `DefaultHasher` is a process-local cache key, not a security boundary — it is
/// not stable across processes and is not meant to be. That is fine: session
/// state dies with the process anyway.
pub fn key_from_messages(messages: &[ChatMessage]) -> Option<SessionKey> {
    let opening = crate::proto::openai::first_user_text(messages);
    if opening.trim().is_empty() {
        return None;
    }
    let mut h = DefaultHasher::new();
    opening.hash(&mut h);
    Some(SessionKey::MessageHash(h.finish()))
}

/// A decision worth replaying on the next turn of the same conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredDecision {
    pub legs: Vec<String>,
    /// What produced it originally. Kept so an affinity hit can say *what* it is
    /// reusing, which is the difference between a useful rationale and "cached".
    pub source: DecisionSource,
    pub label: Option<String>,
    /// §8: the chain, so a replayed decision finds the same gate.
    pub chain: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SessionState {
    /// §6.3 affinity. A `FallOpen` decision is stored too — otherwise a flaky
    /// classifier gets re-hammered every single turn, which is the opposite of
    /// what affinity is for.
    pub decision: Option<StoredDecision>,
    /// §4.3 + §6.3: backends that overflowed on this conversation. Accumulates
    /// for the session's life and is **never cleared**: after a client truncates
    /// or resets its context it should send a *new* session id, which is cheaper
    /// and less surprising than us guessing when the prompt got shorter.
    pub excluded: HashSet<String>,
    /// §8: consecutive judge escalations. Consecutive is load-bearing — a decline
    /// resets it, and a judge *outage* holds it rather than clearing it, so an
    /// outage can never manufacture a latch.
    pub escalate_streak: u32,
    /// §8: once the judge has confirmed, later turns skip both the cheap leg and
    /// the judge. Names the leg that was escalated *past*.
    pub latched: Option<String>,
    last_seen: Instant,
}

impl SessionState {
    fn new(now: Instant) -> SessionState {
        SessionState {
            decision: None,
            excluded: HashSet::new(),
            escalate_streak: 0,
            latched: None,
            last_seen: now,
        }
    }
}

/// A bounded map of conversation state.
///
/// **A client-supplied header keying an unbounded map is a memory-exhaustion
/// vector**, so both caps are enforced: a TTL sweep amortized onto insert (no
/// background thread), plus hard eviction of the least-recently-seen entry at
/// `max_entries`.
pub struct SessionStore {
    inner: Mutex<HashMap<SessionKey, SessionState>>,
    max_entries: usize,
    ttl: Duration,
}

impl SessionStore {
    pub fn new(max_entries: usize, ttl_secs: u64) -> SessionStore {
        SessionStore {
            inner: Mutex::new(HashMap::new()),
            // A zero cap would mean "store nothing", which reads as a config typo
            // rather than an intent; one entry is the smallest honest map.
            max_entries: max_entries.max(1),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// Read a session's state, refreshing its last-seen stamp.
    pub fn get(&self, key: &SessionKey) -> Option<SessionState> {
        let now = Instant::now();
        let mut m = self.lock();
        let st = m.get_mut(key)?;
        if now.duration_since(st.last_seen) > self.ttl {
            // Expired but not yet swept: treat it as absent and drop it now, so a
            // stale decision can never be replayed.
            m.remove(key);
            return None;
        }
        st.last_seen = now;
        Some(st.clone())
    }

    /// Mutate (creating if absent) under the lock, enforcing both bounds first.
    pub fn update<F: FnOnce(&mut SessionState)>(&self, key: &SessionKey, f: F) {
        let now = Instant::now();
        let mut m = self.lock();
        self.evict(&mut m, now, key);
        let st = m
            .entry(key.clone())
            .or_insert_with(|| SessionState::new(now));
        st.last_seen = now;
        f(st);
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Sweep expired entries, then hard-evict the oldest until there is room for
    /// `incoming`. Both bounds run on the insert path: the sweep is amortized
    /// (every insert pays a scan proportional to the live set, which is capped),
    /// and the hard eviction is what makes the cap a cap rather than a hope.
    fn evict(
        &self,
        m: &mut HashMap<SessionKey, SessionState>,
        now: Instant,
        incoming: &SessionKey,
    ) {
        m.retain(|_, st| now.duration_since(st.last_seen) <= self.ttl);
        if m.contains_key(incoming) {
            return;
        }
        while m.len() >= self.max_entries {
            let Some(oldest) = m
                .iter()
                .min_by_key(|(_, st)| st.last_seen)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            m.remove(&oldest);
        }
    }

    /// A poisoned session map must not brick routing: affinity is an
    /// optimization, and the worst a recovered guard can do is replay a decision
    /// that was mid-write. Nothing under this lock can panic (no logging, no
    /// allocation that can fail observably), so there is no torn state to fear.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionKey, SessionState>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    fn msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: crate::proto::openai::MessageContent::Text(text.to_string()),
            name: None,
        }
    }

    #[test]
    fn header_derivation_prefers_our_own_id_then_the_harness_natives() {
        assert_eq!(
            key_from_headers(&hdrs(&[
                ("x-conversation-id", "continue-1"),
                ("x-tmuxlet-session-id", "ours"),
            ])),
            Some(SessionKey::Header("ours".into())),
            "our own header wins regardless of arrival order"
        );
        assert_eq!(
            key_from_headers(&hdrs(&[("x-claude-code-session-id", "cc-7")])),
            Some(SessionKey::Header("cc-7".into()))
        );
        assert_eq!(key_from_headers(&hdrs(&[("x-other", "z")])), None);
        // A present-but-blank header is not an id; treating "" as a session would
        // merge every such client into one bucket.
        assert_eq!(key_from_headers(&hdrs(&[("x-session-id", "   ")])), None);
    }

    #[test]
    fn message_hash_is_stable_within_a_process_and_differs_by_opening() {
        let a = vec![msg("user", "Refactor auth"), msg("user", "go on")];
        let b = vec![msg("user", "Refactor auth"), msg("user", "different tail")];
        let c = vec![msg("user", "Something else")];
        assert_eq!(
            key_from_messages(&a),
            key_from_messages(&b),
            "keyed on the OPENING message, so later turns still hit"
        );
        assert_ne!(key_from_messages(&a), key_from_messages(&c));
        assert_eq!(
            key_from_messages(&[msg("system", "x")]),
            None,
            "no user turn, no key"
        );
    }

    #[test]
    fn a_decision_is_stored_and_replayed() {
        let s = SessionStore::new(16, 7200);
        let k = SessionKey::Header("k".into());
        assert!(s.get(&k).is_none());
        s.update(&k, |st| {
            st.decision = Some(StoredDecision {
                legs: vec!["a".into(), "b".into()],
                source: DecisionSource::Classifier,
                label: Some("code/do-code".into()),
                chain: Some("do-code".into()),
            });
        });
        let got = s.get(&k).unwrap().decision.unwrap();
        assert_eq!(got.legs, vec!["a", "b"]);
        assert_eq!(got.source, DecisionSource::Classifier);
    }

    #[test]
    fn exclusions_accumulate_and_are_never_cleared() {
        // §6.3: after a client truncates its context it should send a NEW session
        // id. Guessing when the prompt got short enough again is exactly the kind
        // of cleverness that produces a mystery 400 six turns later.
        let s = SessionStore::new(16, 7200);
        let k = SessionKey::Header("k".into());
        s.update(&k, |st| {
            st.excluded.insert("small-model".into());
        });
        s.update(&k, |st| {
            st.excluded.insert("other".into());
        });
        let ex = s.get(&k).unwrap().excluded;
        assert!(ex.contains("small-model") && ex.contains("other"));
    }

    #[test]
    fn the_entry_cap_is_a_cap_under_a_synthetic_key_flood() {
        // The §11 gate for Phase 2: "Memory bound proven under a synthetic key
        // flood." A client-supplied header keying an unbounded map is a
        // memory-exhaustion vector, so this is the test that matters.
        let s = SessionStore::new(64, 7200);
        for i in 0..10_000 {
            let k = SessionKey::Header(format!("flood-{i}"));
            s.update(&k, |st| {
                st.excluded.insert("x".into());
            });
        }
        assert!(s.len() <= 64, "store grew past its cap: {}", s.len());
        // The most recent key survived; the oldest did not.
        assert!(s.get(&SessionKey::Header("flood-9999".into())).is_some());
        assert!(s.get(&SessionKey::Header("flood-0".into())).is_none());
    }

    #[test]
    fn an_expired_entry_is_never_replayed() {
        let s = SessionStore::new(16, 0); // ttl 0 => everything is stale at once
        let k = SessionKey::Header("k".into());
        s.update(&k, |st| {
            st.decision = Some(StoredDecision {
                legs: vec!["a".into()],
                source: DecisionSource::Classifier,
                label: None,
                chain: None,
            });
        });
        // The stamp is set on write, so a 0s TTL means the very next read is past
        // it. `>` not `>=` on the comparison would make this flaky, not failing —
        // hence asserting the absence rather than the count.
        std::thread::sleep(Duration::from_millis(2));
        assert!(s.get(&k).is_none(), "a stale decision must not be replayed");
    }

    #[test]
    fn refreshing_an_existing_key_does_not_evict_it() {
        // The insert path evicts to make room for a NEW key; updating a key that
        // is already resident must not be able to evict itself.
        let s = SessionStore::new(1, 7200);
        let k = SessionKey::Header("only".into());
        s.update(&k, |st| {
            st.excluded.insert("a".into());
        });
        s.update(&k, |st| {
            st.excluded.insert("b".into());
        });
        assert_eq!(s.get(&k).unwrap().excluded.len(), 2);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn session_labels_are_bounded_and_header_safe() {
        // The header form is attacker-supplied and ends up in a log line and an
        // echo header, so it gets the same treatment as any other dynamic value.
        let k = SessionKey::Header(format!("sess-\n{}", "x".repeat(200)));
        let l = k.label();
        assert!(l.chars().count() <= 64, "unbounded label: {l}");
        assert!(!l.contains('\n'), "control chars must not survive: {l:?}");
        assert_eq!(
            SessionKey::MessageHash(0xdead).label(),
            "hash-000000000000dead"
        );
    }
}

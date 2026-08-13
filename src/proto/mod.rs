//! Wire protocols the server speaks, plus the request shaping that is common to
//! all of them.
//!
//! `openai` holds the `/v1/chat/completions` request and response shapes. Phase 5
//! adds `anthropic` beside it. `PromptMode` lives here rather than in either one
//! because it describes how a parsed conversation is flattened for a *backend* —
//! that is a property of the backend, not of the protocol the client used.

pub mod openai;

/// U-14: how a backend shapes the request into a single prompt string.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PromptMode {
    /// Full role-labeled transcript (default; current behavior).
    #[default]
    Transcript,
    /// Final user message verbatim, system messages prepended plainly.
    LastUser,
}

impl PromptMode {
    /// Parse a config string; `None` for unknown values (validate() reports them).
    pub fn parse(s: &str) -> Option<PromptMode> {
        match s {
            "transcript" => Some(PromptMode::Transcript),
            "last_user" => Some(PromptMode::LastUser),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_mode_parses_known_values_only() {
        // U-14: an unknown value is `None` here and reported by config::validate,
        // rather than silently defaulting to transcript.
        assert_eq!(
            PromptMode::parse("transcript"),
            Some(PromptMode::Transcript)
        );
        assert_eq!(PromptMode::parse("last_user"), Some(PromptMode::LastUser));
        assert_eq!(PromptMode::parse("nope"), None);
        assert_eq!(PromptMode::default(), PromptMode::Transcript);
    }
}

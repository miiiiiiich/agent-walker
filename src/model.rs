use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Combined,
    Claude,
    Codex,
    Agy,
    OpenCode,
    Copilot,
    Grok,
    Cursor,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Combined => "Total",
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Agy => "Agy",
            Self::OpenCode => "OpenCode",
            Self::Copilot => "Copilot",
            Self::Grok => "Grok",
            Self::Cursor => "Cursor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceKind {
    Main,
    Subagent,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_ephemeral_1h_input_tokens: u64,
    pub cache_creation_ephemeral_5m_input_tokens: u64,
    pub server_tool_use: BTreeMap<String, u64>,
}

impl TokenUsage {
    // All arithmetic saturates: token counts come from untrusted log files,
    // and a poisoned value must degrade to a pinned number, never wrap or
    // panic the whole dashboard.
    pub fn token_volume(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }

    pub fn add_assign(&mut self, other: &Self) {
        let add = |target: &mut u64, value: u64| *target = target.saturating_add(value);
        add(&mut self.input_tokens, other.input_tokens);
        add(&mut self.output_tokens, other.output_tokens);
        add(
            &mut self.reasoning_output_tokens,
            other.reasoning_output_tokens,
        );
        add(
            &mut self.cache_creation_input_tokens,
            other.cache_creation_input_tokens,
        );
        add(
            &mut self.cache_read_input_tokens,
            other.cache_read_input_tokens,
        );
        add(
            &mut self.cache_creation_ephemeral_1h_input_tokens,
            other.cache_creation_ephemeral_1h_input_tokens,
        );
        add(
            &mut self.cache_creation_ephemeral_5m_input_tokens,
            other.cache_creation_ephemeral_5m_input_tokens,
        );
        for (key, value) in &other.server_tool_use {
            let entry = self.server_tool_use.entry(key.clone()).or_default();
            *entry = entry.saturating_add(*value);
        }
    }
}

mod events;
mod summary;

pub use events::*;
pub use summary::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_arithmetic_saturates_instead_of_wrapping() {
        let poisoned = TokenUsage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            ..TokenUsage::default()
        };
        assert_eq!(poisoned.token_volume(), u64::MAX);

        let mut total = TokenUsage {
            input_tokens: u64::MAX - 1,
            ..TokenUsage::default()
        };
        total.add_assign(&poisoned);
        assert_eq!(total.input_tokens, u64::MAX);
    }
}

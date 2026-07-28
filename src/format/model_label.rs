//! Model-label display names and the sanitize boundary: labels can carry
//! personal-environment strings, and the collapse rules here are what keep
//! them off the share card. This is a SAFETY surface — a diff touching this
//! file changes what can leak into shared output (see SECURITY.md).
pub fn short_model_name(name: &str) -> String {
    sanitize_label(&short_model_name_raw(name))
}

/// Clamp a model label to a safe shape. Model names come from untrusted logs, so
/// a crafted one could smuggle a repo name, an absolute path, or a token-like
/// string onto the shareable card / clipboard — artifacts meant to carry none.
///
/// Stripping disallowed characters is not enough: it would still publish the
/// readable fragments of a smuggled path (`gemini/Users/alice/secret` →
/// `geminiUsersalicesecret`). So a label containing anything outside the
/// model-name character set is treated as suspicious and collapsed to a generic
/// value. Legitimate names (which only use that set) pass through unchanged,
/// capped for layout; well-known families have already collapsed to a constant
/// upstream, so this only ever judges an unrecognized passthrough name.
fn sanitize_label(label: &str) -> String {
    const MAX: usize = 24;
    // Bound the scan: an untrusted name could be megabytes long, and there's no
    // need to examine past the first SCAN characters to make this decision.
    const SCAN: usize = 128;
    let label = label.trim();
    // `:` is allowed so local-model ids keep their tag (Ollama / OpenCode use
    // `qwen3:8b`-style names); path separators (`/`, `\`) stay out, so a smuggled
    // absolute path is still collapsed rather than published.
    let allowed = |ch: char| {
        ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '.' | '-' | '_' | '(' | ')' | '+' | ':')
    };
    // Validate on the iterator (no temp allocation): a label is suspicious if
    // it's empty or any of its first SCAN characters falls outside the set.
    if label.is_empty() || label.chars().take(SCAN).any(|ch| !allowed(ch)) {
        return "Other".to_owned();
    }
    // The label is already end-trimmed; only the MAX cut can leave a trailing
    // space, so trim that in place instead of allocating a second string.
    let mut capped: String = label.chars().take(MAX).collect();
    let trimmed_len = capped.trim_end().len();
    if trimmed_len == 0 {
        return "Other".to_owned();
    }
    capped.truncate(trimmed_len);
    capped
}

/// Strip a *known* `<provider>/` namespace from a gateway/proxy model id (agent
/// runners and routers use `provider/model` ids), so the model shows by its real
/// name. An *unknown* prefix is left intact — the surviving `/` then makes
/// `sanitize_label` collapse it to "Other", which is what keeps an arbitrary
/// `org/repo` (or a path) from reaching the card. A stale list degrades safely: a
/// new provider's model just shows as "Other" until it's added here.
fn strip_known_provider_prefix(name: &str) -> &str {
    const PROVIDERS: &[&str] = &[
        "openai",
        "anthropic",
        "google",
        "vertex_ai",
        "vertex",
        "meta-llama",
        "meta",
        "mistralai",
        "mistral",
        "x-ai",
        "xai",
        "deepseek",
        "qwen",
        "cohere",
        "perplexity",
        "azure",
        "bedrock",
        "openrouter",
        "together",
        "fireworks",
        "groq",
        "ollama",
    ];
    if let Some((prefix, rest)) = name.split_once('/')
        && !rest.is_empty()
        && PROVIDERS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(prefix))
    {
        return rest;
    }
    name
}

fn short_model_name_raw(name: &str) -> String {
    let name = strip_known_provider_prefix(name);
    let lower = name.to_ascii_lowercase();
    let family = if lower.contains("opus") {
        "Opus"
    } else if lower.contains("sonnet") {
        "Sonnet"
    } else if lower.contains("haiku") {
        "Haiku"
    } else if lower.contains("fable") {
        "Fable"
    } else if lower.contains("gemini") {
        "Gemini"
    } else if lower.contains("gpt") {
        return name.replace("gpt-", "GPT ");
    } else if lower == "openai" || lower == "codex" {
        return "Codex".to_owned();
    } else {
        return name.to_owned();
    };

    if family == "Gemini" {
        return name.to_owned();
    }

    for version in ["4-8", "4-7", "4-6", "4-5", "4-1", "5", "4"] {
        if lower.contains(version) {
            return format!("{family} {}", version.replace('-', "."));
        }
    }

    family.to_owned()
}

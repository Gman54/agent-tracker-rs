//! Secret scrubbing for anything that gets stored or displayed.
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{Map, Value};

pub const REDACTED: &str = "[REDACTED]";

static TEXT_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
        r"\b(?:sk|pk|rk)-[A-Za-z0-9_\-]{16,}",
        r"\bsk-ant-[A-Za-z0-9_\-]{16,}",
        r"\baat_[A-Za-z0-9_\-]{16,}",
        r"\bAKIA[0-9A-Z]{16}\b",
        r"\bgh[pousr]_[A-Za-z0-9]{20,}",
        r"\bgithub_pat_[A-Za-z0-9_]{20,}",
        r"\bxox[abprs]-[A-Za-z0-9\-]{10,}",
        r"\bAIza[0-9A-Za-z_\-]{30,}",
        r"\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
        r"(?i)\bbearer\s+[A-Za-z0-9._\-]{16,}",
        r#"(?i)\b(password|passwd|pwd|secret|api[_-]?key|access[_-]?token|token)\s*[:=]\s*[^\s,;"']{4,}"#,
    ]
    .iter()
    .map(|p| Regex::new(p).expect("redaction regex"))
    .collect()
});

static SECRET_KEY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(password|passwd|secret|api[_-]?key|access[_-]?token|auth[_-]?token|^token$|authorization|private[_-]?key|cookie)").unwrap()
});

/// Replaces secret-looking substrings inside free text.
pub fn redact_text(text: &str) -> String {
    let mut out = text.to_string();
    for pat in TEXT_PATTERNS.iter() {
        out = pat
            .replace_all(&out, |c: &regex::Captures| match c.get(1) {
                Some(k) if !k.as_str().is_empty() => format!("{}={REDACTED}", k.as_str()),
                _ => REDACTED.to_string(),
            })
            .into_owned();
    }
    out
}

/// Recursively redacts a JSON value: secret-named scalar fields are blanked, strings are scrubbed.
pub fn redact_deep(value: &Value) -> Value {
    redact_depth(value, 0)
}

fn redact_depth(value: &Value, depth: usize) -> Value {
    if depth > 12 {
        return Value::String("[TRUNCATED]".into());
    }
    match value {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, v) in m {
                let scalar = matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_));
                if scalar && SECRET_KEY_RE.is_match(k) && !k.ends_with("_count") && k != "tokens_in" && k != "tokens_out" {
                    out.insert(k.clone(), Value::String(REDACTED.into()));
                } else {
                    out.insert(k.clone(), redact_depth(v, depth + 1));
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().take(1000).map(|v| redact_depth(v, depth + 1)).collect()),
        Value::String(s) => Value::String(redact_text(s)),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scrubs_keys_and_text() {
        let v = json!({"api_key": "abc123456", "cmd": "export OPENAI_API_KEY=sk-abcdefghijklmnopqrstuv", "n": 3});
        let r = redact_deep(&v);
        assert_eq!(r["api_key"], REDACTED);
        assert!(!r["cmd"].as_str().unwrap().contains("sk-abcdefghijklmnop"));
        assert_eq!(r["n"], 3);
    }
}

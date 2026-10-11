use crate::config::Rule;
use serde_json::Value;

/// What every agent's output is normalised into.
#[derive(Debug)]
pub enum Ev {
    Text(String),
    Session(String),
    Usage { prompt: u64, completion: u64 },
}

/// Apply an agent's rules to one line of its JSON output.
pub fn apply(rules: &[Rule], line: &str) -> Vec<Ev> {
    let Ok(json) = serde_json::from_str::<Value>(line) else {
        return Vec::new(); // banners, update notices, blank lines
    };
    let mut out = Vec::new();
    for rule in rules {
        if !rule.when.iter().all(|(p, want)| json.pointer(p).is_some_and(|v| equals(v, want))) {
            continue;
        }
        if !rule.require.iter().all(|p| json.pointer(p).is_some_and(|v| !v.is_null())) {
            continue;
        }
        if rule.absent.iter().any(|p| json.pointer(p).is_some_and(|v| !v.is_null())) {
            continue;
        }
        if let Some(t) = rule.text.as_deref().and_then(|p| json.pointer(p)).and_then(Value::as_str)
            && !t.is_empty() {
                out.push(Ev::Text(t.to_string()));
            }
        if let Some(s) = rule.session.as_deref().and_then(|p| json.pointer(p)).and_then(Value::as_str)
            && !s.is_empty() {
                out.push(Ev::Session(s.to_string()));
            }
        let num = |p: &Option<String>| p.as_deref().and_then(|p| json.pointer(p)).and_then(Value::as_u64);
        if rule.prompt_tokens.is_some() || rule.completion_tokens.is_some() {
            out.push(Ev::Usage {
                prompt: num(&rule.prompt_tokens).unwrap_or(0),
                completion: num(&rule.completion_tokens).unwrap_or(0),
            });
        }
    }
    out
}

fn equals(v: &Value, want: &str) -> bool {
    match v {
        Value::String(s) => s == want,
        Value::Null => false,
        // numbers and bools are compared by their JSON text ("false", "3");
        // `other == want` would compile but never match a non-string value
        #[allow(clippy::cmp_owned)]
        other => other.to_string() == want,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn texts(agent: &str, lines: &[&str]) -> String {
        let cfg = Config::load(None, &Default::default()).unwrap();
        let rules = &cfg.agents[agent].rules;
        lines
            .iter()
            .flat_map(|l| apply(rules, l))
            .filter_map(|e| if let Ev::Text(t) = e { Some(t) } else { None })
            .collect()
    }

    #[test]
    fn cursor_skips_repeated_full_text() {
        let out = texts(
            "agent",
            &[
                r#"{"type":"assistant","message":{"content":[{"text":"Hel"}]},"timestamp_ms":1}"#,
                r#"{"type":"assistant","message":{"content":[{"text":"lo"}]},"timestamp_ms":2}"#,
                // recap before a tool call
                r#"{"type":"assistant","message":{"content":[{"text":"Hello"}]},"timestamp_ms":3,"model_call_id":"x"}"#,
                // final full message
                r#"{"type":"assistant","message":{"content":[{"text":"Hello"}]}}"#,
            ],
        );
        assert_eq!(out, "Hello");
    }

    #[test]
    fn cmd_reads_text_deltas_only() {
        let out = texts(
            "cmd",
            &[
                "Updated 1.77.0 -> 1.79.2",
                r#"{"type":"event","event":{"type":"text_delta","delta":"hi"}}"#,
                r#"{"type":"event","event":{"type":"message_update","content":[{"text":"hi"}]}}"#,
            ],
        );
        assert_eq!(out, "hi");
    }
}

#[cfg(test)]
mod equals_tests {
    use super::equals;
    use serde_json::json;

    #[test]
    fn non_string_values_match_by_json_text() {
        assert!(equals(&json!(false), "false"));
        assert!(equals(&json!(3), "3"));
        assert!(equals(&json!("x"), "x"));
        assert!(!equals(&json!(null), "null"));
    }
}

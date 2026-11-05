//! The secret values a turn handed its agent, scrubbed by value from everything the turn streams,
//! records, or exports before it leaves the loop pod.

use crucible_contract::event::{AgentEvent, RawStream};
use serde_json::Value;
use std::borrow::Cow;
use std::sync::{Arc, PoisonError, RwLock};

/// What a scrubbed secret reads as.
pub const MASK: &str = "[REDACTED]";

/// Shorter values are not registered: masking them would mangle ordinary output.
pub const MIN_SECRET_CHARS: usize = 8;

/// One turn's secret values. Clones share the set, so a value registered after a consumer took
/// its clone (a token re-minted mid-turn) is scrubbed there too.
#[derive(Clone, Default)]
pub struct Redactor {
    secrets: Arc<RwLock<Vec<Box<str>>>>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field("secrets", &self.read().len())
            .finish()
    }
}

impl Redactor {
    /// Register `secret`, and its JSON-escaped spelling when that differs, so a raw JSON line
    /// carrying it is scrubbed as well as a decoded string.
    pub fn add(&self, secret: &str) {
        let secret = secret.trim();
        if secret.chars().count() < MIN_SECRET_CHARS {
            return;
        }
        let escaped = serde_json::to_string(secret)
            .ok()
            .and_then(|quoted| Some(quoted.strip_prefix('"')?.strip_suffix('"')?.to_string()));
        let mut secrets = self.secrets.write().unwrap_or_else(PoisonError::into_inner);
        for value in std::iter::once(secret).chain(escaped.as_deref()) {
            if !secrets.iter().any(|s| &**s == value) {
                secrets.push(value.into());
            }
        }
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<Box<str>>> {
        self.secrets.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    /// `s` with every registered value masked, longest first.
    pub fn text<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(s);
        for secret in self.read().iter() {
            if out.contains(&**secret) {
                out = Cow::Owned(out.replace(&**secret, MASK));
            }
        }
        out
    }

    /// Mask every string in `value`, object keys included. Returns whether anything changed.
    pub fn json(&self, value: &mut Value) -> bool {
        match value {
            Value::String(s) => match self.text(s) {
                Cow::Owned(scrubbed) => {
                    *s = scrubbed;
                    true
                }
                Cow::Borrowed(_) => false,
            },
            Value::Array(items) => items
                .iter_mut()
                .fold(false, |changed, item| self.json(item) | changed),
            Value::Object(map) => {
                let mut changed = map
                    .values_mut()
                    .fold(false, |changed, item| self.json(item) | changed);
                let dirty: Vec<String> = map
                    .keys()
                    .filter(|key| matches!(self.text(key), Cow::Owned(_)))
                    .cloned()
                    .collect();
                for key in dirty {
                    if let Some(item) = map.remove(&key) {
                        map.insert(self.text(&key).into_owned(), item);
                        changed = true;
                    }
                }
                changed
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => false,
        }
    }

    /// `ev` with every string field masked. An event that cannot round-trip through its own
    /// serde shape is withheld rather than passed on unscrubbed.
    pub fn event<'e>(&self, ev: &'e AgentEvent) -> Cow<'e, AgentEvent> {
        if self.is_empty() {
            return Cow::Borrowed(ev);
        }
        let Ok(mut value) = serde_json::to_value(ev) else {
            return Cow::Owned(withheld());
        };
        if !self.json(&mut value) {
            return Cow::Borrowed(ev);
        }
        Cow::Owned(serde_json::from_value(value).unwrap_or_else(|_| withheld()))
    }
}

fn withheld() -> AgentEvent {
    AgentEvent::Raw {
        text: "event withheld: it carried a secret and did not re-encode".to_string(),
        stream: RawStream::Stderr,
    }
}

#[cfg(test)]
mod tests {
    use crate::redact::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    const KEY: &str = "sk-oai-0123456789abcdef";

    fn redactor(secrets: &[&str]) -> Redactor {
        let r = Redactor::default();
        for s in secrets {
            r.add(s);
        }
        r
    }

    #[test]
    fn a_registered_value_is_masked_wherever_it_appears_in_text() {
        let r = redactor(&[KEY]);
        assert_eq!(
            r.text(&format!("curl -H 'x-api-key: {KEY}' https://x; echo {KEY}")),
            format!("curl -H 'x-api-key: {MASK}' https://x; echo {MASK}")
        );
        assert!(matches!(r.text("nothing secret"), Cow::Borrowed(_)));
    }

    #[test]
    fn values_too_short_to_mask_safely_are_not_registered() {
        let r = redactor(&["", "   ", "1234567", " 1234567 "]);
        assert!(r.is_empty());
        assert_eq!(r.text("1234567"), "1234567");
        r.add(" 12345678\n");
        assert_eq!(r.text("x12345678x"), format!("x{MASK}x"));
    }

    #[test]
    fn a_longer_secret_is_masked_whole_before_a_shorter_one_it_contains() {
        let r = redactor(&["abcdefgh", "abcdefgh-ijklmnop"]);
        assert_eq!(
            r.text("abcdefgh-ijklmnop abcdefgh"),
            format!("{MASK} {MASK}")
        );
    }

    #[test]
    fn the_json_escaped_spelling_is_masked_in_a_raw_json_line() {
        let secret = "pa\"ss\\word-01";
        let r = redactor(&[secret]);
        let line = serde_json::to_string(&json!({ "cmd": format!("echo {secret}") })).unwrap();
        let scrubbed = r.text(&line);
        assert!(!scrubbed.contains("ss\\\\word"), "{scrubbed}");
        assert_eq!(
            serde_json::from_str::<Value>(&scrubbed).unwrap()["cmd"],
            format!("echo {MASK}")
        );
    }

    #[test]
    fn every_string_in_a_json_tree_is_masked_keys_included() {
        let r = redactor(&[KEY]);
        let mut v = json!({
            "headers": [{ "x-api-key": KEY }],
            KEY: 3,
            "n": 1,
            "ok": true,
            "nested": { "deep": [format!("Bearer {KEY}"), null] },
        });
        assert!(r.json(&mut v));
        assert!(!v.to_string().contains(KEY), "{v}");
        assert_eq!(v["headers"][0]["x-api-key"], MASK);
        assert_eq!(v[MASK], 3);
        assert_eq!(v["nested"]["deep"][0], format!("Bearer {MASK}"));
        assert_eq!(v["n"], 1);
        let mut clean = json!({ "a": ["b", 1] });
        assert!(!r.json(&mut clean));
    }

    #[test]
    fn every_event_shape_comes_back_masked() {
        let r = redactor(&[KEY]);
        let events = [
            AgentEvent::Tool {
                name: "Bash".into(),
                summary: format!("curl -H 'x-api-key: {KEY}'"),
                subagent: false,
                input: Some(json!({ "command": format!("echo {KEY}") })),
                result: Some(KEY.into()),
                failed: false,
            },
            AgentEvent::Text {
                delta: format!("the key is {KEY}"),
            },
            AgentEvent::Raw {
                text: KEY.into(),
                stream: RawStream::Stderr,
            },
            AgentEvent::Error {
                error_type: "x".into(),
                message: KEY.into(),
            },
            AgentEvent::SandboxLog {
                ts_ms: 1,
                level: "INFO".into(),
                target: "proxy".into(),
                message: KEY.into(),
                denial: false,
                fields: BTreeMap::from([("header".to_string(), KEY.to_string())]),
            },
        ];
        for ev in &events {
            let scrubbed = r.event(ev);
            assert!(matches!(scrubbed, Cow::Owned(_)), "{ev:?}");
            let encoded = serde_json::to_string(scrubbed.as_ref()).unwrap();
            assert!(!encoded.contains(KEY), "{encoded}");
            assert!(encoded.contains(MASK), "{encoded}");
        }
        let clean = AgentEvent::Text {
            delta: "fine".into(),
        };
        assert!(matches!(r.event(&clean), Cow::Borrowed(_)));
        assert!(matches!(
            Redactor::default().event(&events[0]),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn a_clone_scrubs_values_registered_after_it_was_taken() {
        let r = Redactor::default();
        let early = r.clone();
        r.add(KEY);
        assert_eq!(early.text(KEY), MASK);
    }

    #[test]
    fn debug_shows_a_count_never_a_value() {
        let r = redactor(&[KEY]);
        let shown = format!("{r:?}");
        assert!(!shown.contains(KEY), "{shown}");
        assert!(shown.contains("secrets: 1"), "{shown}");
    }
}

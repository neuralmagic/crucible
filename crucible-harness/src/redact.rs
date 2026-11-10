//! The secret values a turn handed its agent, scrubbed by value from everything the turn streams,
//! records, or exports before it leaves the loop pod; with [`SecretRules`], also the secrets the
//! agent found itself, scrubbed by Kingfisher's built-in rules.

use crucible_contract::event::{AgentEvent, RawStream};
use kingfisher_scanner::{RulesDatabase, Scanner, get_builtin_rules};
use serde_json::Value;
use std::borrow::Cow;
use std::ops::Range;
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

/// What a scrubbed secret reads as.
pub const MASK: &str = "[REDACTED]";

/// Shorter values are not registered: masking them would mangle ordinary output.
pub const MIN_SECRET_CHARS: usize = 8;

/// Kingfisher's built-in secret rules, compiled on first use. Clones share the compile; a failed
/// compile masks every string.
#[derive(Clone, Default)]
pub struct SecretRules(Arc<OnceLock<Option<Scanner>>>);

impl std::fmt::Debug for SecretRules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretRules")
            .field("compiled", &self.0.get().is_some())
            .finish()
    }
}

fn compile() -> Option<Scanner> {
    let db = RulesDatabase::from_rule_collection(get_builtin_rules(None).ok()?).ok()?;
    Some(Scanner::new(Arc::new(db)))
}

impl SecretRules {
    /// Compile the rules now if they have not been; whether they compiled.
    pub fn ready(&self) -> bool {
        self.0.get_or_init(compile).is_some()
    }

    /// The byte ranges of every finding in `s`, widened to char boundaries, sorted and merged;
    /// `None` when the rules or the scan fail.
    fn spans(&self, s: &str) -> Option<Vec<Range<usize>>> {
        let scanner = self.0.get_or_init(compile).as_ref()?;
        let mut spans: Vec<Range<usize>> = scanner
            .scan_bytes(s.as_bytes())
            .ok()?
            .iter()
            .map(|f| {
                let start = s.floor_char_boundary(f.location.start_offset);
                let end = s.ceil_char_boundary(f.location.end_offset);
                start..end
            })
            .filter(|span| !span.is_empty())
            .collect();
        spans.sort_by_key(|span| span.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(spans.len());
        for span in spans {
            match merged.last_mut() {
                Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
                _ => merged.push(span),
            }
        }
        Some(merged)
    }

    /// `s` with every finding masked; all of `s` masked when the rules or the scan fail.
    fn mask<'a>(&self, s: Cow<'a, str>) -> Cow<'a, str> {
        let spans = match self.spans(&s) {
            Some(spans) if spans.is_empty() => return s,
            Some(spans) => spans,
            None => return Cow::Owned(MASK.to_string()),
        };
        let mut out = String::with_capacity(s.len());
        let mut at = 0;
        for span in spans {
            out.push_str(&s[at..span.start]);
            out.push_str(MASK);
            at = span.end;
        }
        out.push_str(&s[at..]);
        Cow::Owned(out)
    }
}

/// One turn's secret values, plus the rules that catch secrets nobody registered. Clones share
/// the set, so a value registered after a consumer took its clone (a token re-minted mid-turn) is
/// scrubbed there too.
#[derive(Clone, Default)]
pub struct Redactor {
    secrets: Arc<RwLock<Vec<Box<str>>>>,
    rules: Option<SecretRules>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field("secrets", &self.read().len())
            .field("rules", &self.rules.is_some())
            .finish()
    }
}

impl Redactor {
    /// A redactor that also masks whatever `rules` find.
    pub fn with_rules(rules: SecretRules) -> Self {
        Self {
            secrets: Arc::default(),
            rules: Some(rules),
        }
    }

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

    /// Whether `text` can change nothing: no values registered and no rules.
    pub fn is_empty(&self) -> bool {
        self.rules.is_none() && self.read().is_empty()
    }

    /// `s` with every registered value masked, longest first, then every rule finding.
    pub fn text<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(s);
        for secret in self.read().iter() {
            if out.contains(&**secret) {
                out = Cow::Owned(out.replace(&**secret, MASK));
            }
        }
        match &self.rules {
            Some(rules) if !out.is_empty() => rules.mask(out),
            _ => out,
        }
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
        assert!(shown.contains("rules: false"), "{shown}");
    }

    /// Credentials the agent could find on its own. The PEM key is a throwaway ed25519 key.
    fn found_secrets() -> [(&'static str, String); 5] {
        let label = ["PRIVATE", "KEY"].join(" ");
        [
            (
                "github pat",
                ["ghp", "_R2d9sXbQk7LmP4vT8wYzA1cE6fH3jN0uKoIq"].concat(),
            ),
            ("aws access key id", ["AKIA", "QYLPMN5HHHFPZAM2"].concat()),
            (
                "aws secret key",
                ["1tUm636uS1yOEcfP5pvf", "qJ/ml36mF7AkyHsEU0IU"].concat(),
            ),
            (
                "pem private key",
                format!(
                    "-----BEGIN {label}-----\n\
                     MC4CAQAwBQYDK2VwBCIEIJG2GTB4rI7wQj5WHWdkvIwqT4HU0Kho7NI7c6uTa3vl\n\
                     -----END {label}-----"
                ),
            ),
            (
                "jwt",
                [
                    "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
                    "eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ",
                    "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
                ]
                .join("."),
            ),
        ]
    }

    fn patterns() -> Redactor {
        Redactor::with_rules(SecretRules::default())
    }

    #[test]
    fn found_secrets_are_masked_in_a_tool_input_and_result() {
        let r = patterns();
        assert!(
            !r.is_empty(),
            "patterns alone keep event from short-circuiting"
        );
        let [pat, aws_id, aws_secret, pem, jwt] = found_secrets().map(|(_, s)| s);
        let env = format!("AWS_ACCESS_KEY_ID={aws_id}\nAWS_SECRET_ACCESS_KEY={aws_secret}\n");
        let ev = AgentEvent::Tool {
            name: "Bash".into(),
            summary: "cat ~/.aws/credentials".into(),
            subagent: false,
            input: Some(json!({
                "command": format!("curl -H 'Authorization: token {pat}' https://api.github.com"),
                "env": env,
                "key": pem,
                "headers": { "authorization": format!("Bearer {jwt}") },
            })),
            result: Some(format!("{env}{pem}\ntoken: {jwt}\nGITHUB_TOKEN={pat}\n")),
            failed: false,
        };
        let scrubbed = r.event(&ev);
        assert!(matches!(scrubbed, Cow::Owned(_)));
        let encoded = serde_json::to_string(scrubbed.as_ref()).unwrap();
        for (name, secret) in found_secrets() {
            assert!(!encoded.contains(&secret), "{name} leaked: {encoded}");
        }
        assert!(!encoded.contains("MC4CAQAw"), "pem body leaked: {encoded}");
        let AgentEvent::Tool { input, result, .. } = scrubbed.as_ref() else {
            panic!("{scrubbed:?}");
        };
        let input = input.as_ref().unwrap();
        assert_eq!(input["key"], MASK);
        assert_eq!(
            input["command"],
            format!("curl -H 'Authorization: token {MASK}' https://api.github.com")
        );
        assert!(result.as_ref().unwrap().contains(MASK));
    }

    #[test]
    fn found_secrets_are_masked_in_their_json_escaped_and_base64_spellings() {
        let r = patterns();
        let [.., (_, pem), _] = found_secrets();
        let line = serde_json::to_string(&json!({ "result": pem })).unwrap();
        assert!(line.contains("\\n"), "{line}");
        let scrubbed = r.text(&line);
        assert!(!scrubbed.contains("MC4CAQAw"), "{scrubbed}");
        assert_eq!(
            serde_json::from_str::<Value>(&scrubbed).unwrap()["result"],
            MASK
        );
        let kubeconfig = format!("    client-key-data: {}", base64_encode(pem.as_bytes()));
        assert_eq!(r.text(&kubeconfig), format!("    client-key-data: {MASK}"));
    }

    fn base64_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 63]));
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    #[test]
    fn ordinary_text_and_hashes_pass_through_pattern_scrubbing_unchanged() {
        let r = patterns();
        for clean in [
            "",
            "cargo build -p crucible-harness finished in 44.16s",
            "commit 9c701758a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "uuid 3fa85f64-5717-4562-b3fc-2c963f66afa6, ünïcödé ✓",
            "AWS_REGION=us-east-1 GITHUB_REPOSITORY=neuralmagic/crucible",
        ] {
            assert!(matches!(r.text(clean), Cow::Borrowed(_)), "{clean:?}");
        }
        let ev = AgentEvent::Tool {
            name: "Read".into(),
            summary: "Cargo.lock".into(),
            subagent: false,
            input: Some(json!({ "file_path": "/workspace/Cargo.lock" })),
            result: Some(
                "checksum = \"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\""
                    .into(),
            ),
            failed: false,
        };
        assert!(matches!(r.event(&ev), Cow::Borrowed(_)));
    }

    #[test]
    fn registered_values_are_still_masked_alongside_pattern_findings() {
        let r = patterns();
        r.add(KEY);
        let [(_, pat), ..] = found_secrets();
        let early = r.clone();
        r.add("relayed-secret-value");
        assert_eq!(
            early.text(&format!("{KEY} é {pat} relayed-secret-value")),
            format!("{MASK} é {MASK} {MASK}")
        );
    }
}

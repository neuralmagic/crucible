//! `crucible-playbooks://` links: what one opens, and the link for what is on screen.
//!
//! ```text
//! crucible-playbooks://playbook/<id>
//! crucible-playbooks://playbook/<id>/launch/<key>[?task=<name>]
//! crucible-playbooks://local/<pack>
//! ```
//!
//! A link comes from outside the app, so parsing is strict: one scheme, these shapes only, and
//! every part a short run of printable characters.

pub const SCHEME: &str = "crucible-playbooks";

/// The longest part a link may carry, decoded.
const PART: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    Playbook {
        id: String,
    },
    Launch {
        playbook: String,
        key: String,
        task: Option<String>,
    },
    Local {
        pack: String,
    },
}

impl Link {
    pub fn parse(url: &str) -> Option<Link> {
        let (scheme, rest) = url.trim().split_once("://")?;
        if !scheme.eq_ignore_ascii_case(SCHEME) {
            return None;
        }
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let parts = path
            .trim_end_matches('/')
            .split('/')
            .map(part)
            .collect::<Option<Vec<String>>>()?;
        let task = match query {
            "" => None,
            query => Some(part(query.strip_prefix("task=")?)?),
        };
        match parts.as_slice() {
            [kind, id] if kind == "playbook" && task.is_none() => {
                Some(Link::Playbook { id: id.clone() })
            }
            [kind, id, launch, key] if kind == "playbook" && launch == "launch" => key
                .starts_with(&format!("playbook:{id}:"))
                .then(|| Link::Launch {
                    playbook: id.clone(),
                    key: key.clone(),
                    task,
                }),
            [kind, pack] if kind == "local" && task.is_none() => {
                Some(Link::Local { pack: pack.clone() })
            }
            _ => None,
        }
    }

    pub fn url(&self) -> String {
        match self {
            Link::Playbook { id } => format!("{SCHEME}://playbook/{}", encode(id, b"")),
            Link::Launch {
                playbook,
                key,
                task,
            } => {
                let task = task
                    .as_deref()
                    .map(|task| format!("?task={}", encode(task, b"")))
                    .unwrap_or_default();
                format!(
                    "{SCHEME}://playbook/{}/launch/{}{task}",
                    encode(playbook, b""),
                    encode(key, b"")
                )
            }
            Link::Local { pack } => format!("{SCHEME}://local/{}", encode(pack, b"")),
        }
    }
}

/// One decoded path part or query value: non-empty, at most [`PART`] bytes, printable, and no
/// `/` once decoded.
fn part(raw: &str) -> Option<String> {
    let text = decode(raw)?;
    (!text.is_empty() && text.len() <= PART && text.chars().all(|c| !c.is_control() && c != '/'))
        .then_some(text)
}

/// Percent-encode everything but ASCII alphanumerics, `-._~`, and the bytes in `keep`.
pub fn encode(text: &str, keep: &[u8]) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) || keep.contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Undo percent-encoding; `None` for a malformed escape or bytes that are not UTF-8.
pub fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use crate::link::{Link, decode, encode};

    #[test]
    fn links_round_trip_through_their_urls() {
        let links = [
            Link::Playbook {
                id: "triage".into(),
            },
            Link::Launch {
                playbook: "vllm-agent-fanout".into(),
                key: "playbook:vllm-agent-fanout:01a1ae1c-0bcd".into(),
                task: Some("edit[layer norm]".into()),
            },
            Link::Launch {
                playbook: "triage".into(),
                key: "playbook:triage:01a1b94c".into(),
                task: None,
            },
            Link::Local {
                pack: "playbook".into(),
            },
        ];
        for link in links {
            assert_eq!(
                Link::parse(&link.url()),
                Some(link.clone()),
                "{}",
                link.url()
            );
        }
        assert_eq!(
            Link::Launch {
                playbook: "triage".into(),
                key: "playbook:triage:01a1".into(),
                task: Some("scan".into()),
            }
            .url(),
            "crucible-playbooks://playbook/triage/launch/playbook%3Atriage%3A01a1?task=scan"
        );
        assert_eq!(
            Link::parse("CRUCIBLE-PLAYBOOKS://playbook/triage/"),
            Some(Link::Playbook {
                id: "triage".into()
            })
        );
    }

    #[test]
    fn anything_but_the_known_shapes_is_refused() {
        for bad in [
            "",
            "https://playbook/triage",
            "crucible-playbooks:playbook/triage",
            "crucible-playbooks://",
            "crucible-playbooks://playbook",
            "crucible-playbooks://playbook//launch/x",
            "crucible-playbooks://playbook/triage?task=scan",
            "crucible-playbooks://playbook/triage/launch/playbook%3Aother%3A1",
            "crucible-playbooks://playbook/triage/launch/playbook%3Atriage%3A1?x=1",
            "crucible-playbooks://playbook/triage/launch/playbook%3Atriage%3A1?task=",
            "crucible-playbooks://playbook/tri%2Fage",
            "crucible-playbooks://playbook/tri%0Aage",
            "crucible-playbooks://playbook/tri%ZZage",
            "crucible-playbooks://playbook/tri%FFage",
            "crucible-playbooks://local/a/b",
            "crucible-playbooks://settings/theme",
        ] {
            assert_eq!(Link::parse(bad), None, "{bad}");
        }
        let long = format!("crucible-playbooks://playbook/{}", "a".repeat(257));
        assert_eq!(Link::parse(&long), None);
    }

    #[test]
    fn percent_encoding_round_trips_and_keeps_what_it_is_told() {
        assert_eq!(encode("a b/c:d", b"/"), "a%20b/c%3Ad");
        assert_eq!(decode("a%20b/c%3Ad").as_deref(), Some("a b/c:d"));
        assert_eq!(decode("%E2%9C%93").as_deref(), Some("\u{2713}"));
        assert_eq!(decode("%2"), None);
    }
}

//! `crucible-playbooks://` links: what one opens, and the link for what is on screen.
//!
//! ```text
//! crucible-playbooks://playbook/<id>
//! crucible-playbooks://playbook/<id>/launch/<key>[?task=<name>]
//! crucible-playbooks://local/<pack>
//! crucible-playbooks://decision/<id>
//! ```
//!
//! A link comes from outside the app, so parsing is strict: one scheme, these shapes only, and
//! every part a short run of printable characters.

pub const SCHEME: &str = "crucible-playbooks";
/// The scheme the controller's web links use: `crucible://<profile>/<web path>`, or
/// `crucible://open?url=<web url>` for the profile on that URL's origin.
pub const WEB_SCHEME: &str = "crucible";

/// Which profile a web link names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    Profile(String),
    /// The scheme, host and port of a web URL.
    Origin(String),
}

/// Where in the app a web path lands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Launch {
        playbook: String,
        key: String,
    },
    Playbook {
        id: String,
    },
    Decision {
        id: String,
    },
    Page(crate::Page),
    /// Anything else: the profile's runs.
    Home,
}

impl Route {
    /// The web UI path `path` as a place in the app.
    pub fn of(path: &str) -> Route {
        let parts: Vec<String> = path
            .split('/')
            .filter(|p| !p.is_empty())
            .filter_map(part)
            .collect();
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        match parts.as_slice() {
            ["playbook-runs", key, ..] => match key.split(':').collect::<Vec<_>>().as_slice() {
                ["playbook", playbook, _] => Route::Launch {
                    playbook: playbook.to_string(),
                    key: key.to_string(),
                },
                _ => Route::Home,
            },
            ["playbooks", id] if !matches!(*id, "drafts" | "import") => {
                Route::Playbook { id: id.to_string() }
            }
            ["playbooks", id, "launch"] => Route::Playbook { id: id.to_string() },
            ["decisions", id] => Route::Decision { id: id.to_string() },
            ["approvals"] => Route::Page(crate::Page::Approvals),
            ["schedules" | "webhooks"]
            | ["webhooks", _]
            | ["teams", _, "schedules" | "webhooks"] => Route::Page(crate::Page::Triggers),
            ["secrets"] | ["teams", _, "secrets"] => Route::Page(crate::Page::Secrets),
            ["teams"] | ["teams", _] | ["teams", _, "members"] => Route::Page(crate::Page::Teams),
            ["providers"] | ["teams", _, "providers"] => Route::Page(crate::Page::Providers),
            _ => Route::Home,
        }
    }
}

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
    Decision {
        id: String,
    },
    /// A controller web link, to open in the app on the profile it names.
    Web {
        place: Place,
        route: Route,
    },
}

impl Link {
    pub fn parse(url: &str) -> Option<Link> {
        let (scheme, rest) = url.trim().split_once("://")?;
        if scheme.eq_ignore_ascii_case(WEB_SCHEME) {
            return web(rest);
        }
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
            [kind, id] if kind == "decision" && task.is_none() => {
                Some(Link::Decision { id: id.clone() })
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
            Link::Decision { id } => format!("{SCHEME}://decision/{}", encode(id, b"")),
            Link::Web { .. } => String::new(),
        }
    }
}

/// `crucible://<profile>/<path>` or `crucible://open?url=<web url>`.
fn web(rest: &str) -> Option<Link> {
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, query) = host.split_once('?').unwrap_or((host, ""));
    let (path, query) = match path.split_once('?') {
        Some((path, q)) => (path, q),
        None => (path, query),
    };
    if host.eq_ignore_ascii_case("open") {
        let raw = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("url="))?;
        let target = url::Url::parse(&decode(raw)?).ok()?;
        if !matches!(target.scheme(), "http" | "https") {
            return None;
        }
        return Some(Link::Web {
            place: Place::Origin(target.origin().ascii_serialization()),
            route: Route::of(target.path()),
        });
    }
    let profile = part(host)?;
    crate::profile::valid_name(&profile).then(|| Link::Web {
        place: Place::Profile(profile),
        route: Route::of(path),
    })
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
    use crate::link::{Link, Place, Route, decode, encode};

    #[test]
    fn web_links_name_a_profile_or_an_origin_and_land_on_a_view() {
        let key = "playbook:cve-triage:01a1aa62-8b52-70c8";
        assert_eq!(
            Link::parse(&format!(
                "crucible://mpp/playbook-runs/{}/runs/r1",
                encode(key, b"")
            )),
            Some(Link::Web {
                place: Place::Profile("mpp".into()),
                route: Route::Launch {
                    playbook: "cve-triage".into(),
                    key: key.into()
                }
            })
        );
        assert_eq!(
            Link::parse("crucible://open?url=https%3A%2F%2Fcrucible.example.com%2Fdecisions%2Fd-7"),
            Some(Link::Web {
                place: Place::Origin("https://crucible.example.com".into()),
                route: Route::Decision { id: "d-7".into() }
            })
        );
        assert_eq!(
            Route::of("/teams/infra/schedules"),
            Route::Page(crate::Page::Triggers)
        );
        assert_eq!(Route::of("/approvals"), Route::Page(crate::Page::Approvals));
        assert_eq!(
            Route::of("/playbooks/triage"),
            Route::Playbook {
                id: "triage".into()
            }
        );
        assert_eq!(Route::of("/playbooks/drafts"), Route::Home);
        assert_eq!(Route::of("/playbook-runs/not-a-key"), Route::Home);
        assert_eq!(Route::of(""), Route::Home);
        assert_eq!(
            Link::parse("crucible://open?url=javascript%3Aalert(1)"),
            None
        );
        assert_eq!(Link::parse("crucible://open"), None);
        assert_eq!(Link::parse("crucible://bad name/x"), None);
    }

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
            Link::Decision {
                id: "01a2c3d4-decision".into(),
            },
        ];
        assert_eq!(Link::parse("crucible-playbooks://decision/d1?task=x"), None);
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

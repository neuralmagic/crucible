//! The API keys page's own state: the mint form, what a key's row says, and the MCP clients
//! grouped for signing out.

use crux::dto::{ApiKey, McpClientKind, McpSignIn};
use rune::TextEdit;

const MAX_NAME: usize = 120;
const DEFAULT_DAYS: &str = "90";

pub struct KeyForm {
    pub name: TextEdit,
    pub days: TextEdit,
    pub forever: bool,
}

impl Default for KeyForm {
    fn default() -> Self {
        KeyForm {
            name: TextEdit::new(""),
            days: TextEdit::new(DEFAULT_DAYS),
            forever: false,
        }
    }
}

impl KeyForm {
    /// The name and lifetime in days to mint with, or what is wrong with the form.
    pub fn ask(&self) -> Result<(String, Option<i64>), String> {
        let name = self.name.text.trim();
        if name.is_empty() {
            return Err("Name it".into());
        }
        if name.chars().count() > MAX_NAME {
            return Err(format!("{MAX_NAME} characters at most"));
        }
        if self.forever {
            return Ok((name.to_string(), None));
        }
        match self.days.text.trim().parse::<i64>() {
            Ok(days) if days >= 1 => Ok((name.to_string(), Some(days))),
            _ => Err("Days must be a whole number, 1 or more".into()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyState {
    Live,
    Expired,
    Revoked,
}

impl KeyState {
    /// A revoked key stays revoked whatever its expiry says.
    pub fn of(key: &ApiKey, now: i64) -> KeyState {
        if key.revoked_at.is_some() {
            return KeyState::Revoked;
        }
        let expired = key
            .expires_at
            .as_deref()
            .and_then(|at| at.parse::<jiff::Timestamp>().ok())
            .is_some_and(|at| at.as_second() <= now);
        if expired {
            KeyState::Expired
        } else {
            KeyState::Live
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            KeyState::Live => "active",
            KeyState::Expired => "expired",
            KeyState::Revoked => "revoked",
        }
    }
}

/// The command that points an MCP client at the controller with a freshly minted key.
pub fn mcp_install(secret: &str, mcp_url: &str) -> String {
    format!(
        "claude mcp add --transport http crucible {mcp_url} --header \"Authorization: Bearer {secret}\""
    )
}

/// One MCP client's live approvals.
#[derive(Debug, PartialEq, Eq)]
pub struct Client<'a> {
    pub id: &'a str,
    pub label: String,
    pub sign_ins: Vec<&'a McpSignIn>,
}

/// A client's name with who vouches for it, so a self-registered client cannot pass for a
/// registered one.
fn label(sign_in: &McpSignIn) -> String {
    match (sign_in.client_kind, &sign_in.client_host) {
        (McpClientKind::Dynamic, _) => format!("{} (self-registered)", sign_in.client_name),
        (_, Some(host)) => format!("{} ({host})", sign_in.client_name),
        (_, None) => sign_in.client_name.clone(),
    }
}

/// The approvals grouped by client, clients in label order, each client's approvals as listed.
pub fn by_client(sign_ins: &[McpSignIn]) -> Vec<Client<'_>> {
    let mut clients: Vec<Client<'_>> = Vec::new();
    for sign_in in sign_ins {
        match clients.iter_mut().find(|c| c.id == sign_in.client_id) {
            Some(client) => client.sign_ins.push(sign_in),
            None => clients.push(Client {
                id: &sign_in.client_id,
                label: label(sign_in),
                sign_ins: vec![sign_in],
            }),
        }
    }
    clients.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.id.cmp(b.id)));
    clients
}

#[cfg(test)]
mod tests {
    use crux::dto::{ApiKey, McpClientKind, McpSignIn};
    use rune::TextEdit;

    use crate::apikeys::{KeyForm, KeyState, by_client, mcp_install};

    fn key(expires_at: Option<&str>, revoked_at: Option<&str>) -> ApiKey {
        ApiKey {
            id: "k1".into(),
            name: "laptop".into(),
            created_at: "2026-11-01T00:00:00Z".into(),
            expires_at: expires_at.map(str::to_string),
            last_used_at: None,
            revoked_at: revoked_at.map(str::to_string),
        }
    }

    fn at(stamp: &str) -> i64 {
        stamp.parse::<jiff::Timestamp>().unwrap().as_second()
    }

    fn form(name: &str, days: &str, forever: bool) -> KeyForm {
        KeyForm {
            name: TextEdit::new(name),
            days: TextEdit::new(days),
            forever,
        }
    }

    fn sign_in(id: &str, name: &str, host: Option<&str>, kind: McpClientKind) -> McpSignIn {
        McpSignIn {
            client_id: id.into(),
            client_name: name.into(),
            client_host: host.map(str::to_string),
            client_kind: kind,
            resource: format!("https://mcp.example/{id}"),
            cluster: "hub".into(),
            approved_at: "2026-11-02T00:00:00Z".into(),
            last_used_at: "2026-11-09T00:00:00Z".into(),
        }
    }

    #[test]
    fn a_key_is_live_until_its_expiry_passes_and_revoked_whatever_its_expiry() {
        let now = at("2026-11-10T00:00:00Z");
        assert_eq!(KeyState::of(&key(None, None), now), KeyState::Live);
        assert_eq!(
            KeyState::of(&key(Some("2026-11-10T00:00:01Z"), None), now),
            KeyState::Live
        );
        assert_eq!(
            KeyState::of(&key(Some("2026-11-10T00:00:00Z"), None), now),
            KeyState::Expired
        );
        assert_eq!(
            KeyState::of(
                &key(Some("2027-01-01T00:00:00Z"), Some("2026-11-05T00:00:00Z")),
                now
            ),
            KeyState::Revoked
        );
        assert_eq!(
            KeyState::of(&key(Some("not a time"), None), now),
            KeyState::Live
        );
    }

    #[test]
    fn the_form_asks_for_a_trimmed_name_and_a_lifetime_of_whole_days_or_none() {
        assert_eq!(
            form("  ci  ", "30", false).ask(),
            Ok(("ci".to_string(), Some(30)))
        );
        assert_eq!(form("ci", "junk", true).ask(), Ok(("ci".to_string(), None)));
        assert!(form("   ", "30", false).ask().is_err());
        assert!(form(&"n".repeat(121), "30", false).ask().is_err());
        assert!(form(&"n".repeat(120), "30", false).ask().is_ok());
        for days in ["0", "-3", "1.5", "", "soon"] {
            assert!(form("ci", days, false).ask().is_err(), "{days:?}");
        }
        let fresh = KeyForm::default();
        assert_eq!((fresh.days.text.as_str(), fresh.forever), ("90", false));
    }

    #[test]
    fn approvals_group_by_client_in_label_order_and_say_who_vouches() {
        let live = [
            sign_in("zed", "Zed", Some("zed.dev"), McpClientKind::Metadata),
            sign_in("dyn-1", "Editor", None, McpClientKind::Dynamic),
            sign_in("cli", "crux", None, McpClientKind::Registered),
            sign_in("zed", "Zed", Some("zed.dev"), McpClientKind::Metadata),
        ];
        let clients = by_client(&live);
        let shown: Vec<(&str, &str, usize)> = clients
            .iter()
            .map(|c| (c.id, c.label.as_str(), c.sign_ins.len()))
            .collect();
        assert_eq!(
            shown,
            [
                ("dyn-1", "Editor (self-registered)", 1),
                ("zed", "Zed (zed.dev)", 2),
                ("cli", "crux", 1),
            ]
        );
        assert!(by_client(&[]).is_empty());
    }

    #[test]
    fn the_install_command_carries_the_url_and_the_secret() {
        assert_eq!(
            mcp_install("crk_x", "https://mcp.example/mcp"),
            "claude mcp add --transport http crucible https://mcp.example/mcp --header \"Authorization: Bearer crk_x\""
        );
    }
}

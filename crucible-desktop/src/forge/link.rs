//! Linking a signed-in forge account to the person at a controller.

use serde::Deserialize;

use crate::forge::Account;
use crate::forge::device::Tokens;
use crate::net::Handle;
use crate::profile::Target;

#[derive(Deserialize)]
struct Linked {
    login: String,
}

fn controller(target: &Target) -> Result<crux::Client, String> {
    let config = target.config()?;
    let wire = crux::client::HttpTransport::with_client(&config, target.http()?)
        .map_err(|e| format!("{e:#}"))?;
    Ok(crux::Client::new(std::sync::Arc::new(wire)))
}

/// Why a link did not land: refused by the controller, so asking again changes nothing, or
/// failed on the way, so a later try may land.
#[derive(Debug, PartialEq, Eq)]
pub enum LinkError {
    Refused(String),
    Failed(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::Refused(why) | LinkError::Failed(why) => f.write_str(why),
        }
    }
}

impl From<String> for LinkError {
    fn from(why: String) -> Self {
        LinkError::Failed(why)
    }
}

fn answered(e: &anyhow::Error) -> LinkError {
    let refused = e
        .downcast_ref::<crux::client::HttpStatus>()
        .is_some_and(|s| matches!(s.status.as_u16(), 400 | 403 | 404 | 409));
    match refused {
        true => LinkError::Refused(format!("{e:#}")),
        false => LinkError::Failed(format!("{e:#}")),
    }
}

pub fn link(
    runtime: &Handle,
    target: &Target,
    account: &Account,
    tokens: &Tokens,
) -> Result<String, LinkError> {
    let client = controller(target)?;
    let body = serde_json::json!({ "token": tokens.access() });
    let linked: Linked = runtime
        .block_on(crate::net::bounded(
            &format!("linking the {} account", account.name()),
            client.put(&account.link_path(), &body),
        ))?
        .map_err(|e| answered(&e))?;
    Ok(linked.login)
}

pub fn unlink(runtime: &Handle, target: &Target, account: &Account) -> Result<(), String> {
    let client = controller(target)?;
    runtime.block_on(crate::net::within(
        &format!("unlinking the {} account", account.name()),
        client.delete(&account.link_path()),
    ))
}

#[cfg(test)]
mod tests {
    use crate::forge::link::*;

    fn status(code: u16) -> anyhow::Error {
        let client = crux::client::HttpStatus::new(
            "/api/me/forge-accounts/github/github.com",
            reqwest::StatusCode::from_u16(code).unwrap(),
            "{\"error\":\"no\"}",
        );
        anyhow::Error::new(client)
    }

    #[test]
    fn a_controller_refusal_is_final_and_anything_else_may_be_retried() {
        for code in [400, 403, 404, 409] {
            assert!(
                matches!(answered(&status(code)), LinkError::Refused(_)),
                "{code}"
            );
        }
        for code in [401, 429, 500, 503] {
            assert!(
                matches!(answered(&status(code)), LinkError::Failed(_)),
                "{code}"
            );
        }
        assert!(matches!(
            answered(&anyhow::Error::new(std::io::Error::other(
                "connection refused"
            ))),
            LinkError::Failed(_)
        ));
    }
}

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

pub fn link(
    runtime: &Handle,
    target: &Target,
    account: &Account,
    tokens: &Tokens,
) -> Result<String, String> {
    let client = controller(target)?;
    let body = serde_json::json!({ "token": tokens.access() });
    let linked: Linked = runtime.block_on(crate::net::within(
        &format!("linking the {} account", account.name()),
        client.put(&account.link_path(), &body),
    ))?;
    Ok(linked.login)
}

pub fn unlink(runtime: &Handle, target: &Target, account: &Account) -> Result<(), String> {
    let client = controller(target)?;
    runtime.block_on(crate::net::within(
        &format!("unlinking the {} account", account.name()),
        client.delete(&account.link_path()),
    ))
}

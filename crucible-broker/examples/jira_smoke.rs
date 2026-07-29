//! Smoke-test the JIRA client against a REAL Atlassian instance: the config surface for "does my
//! setup work end to end". It drives the exact same [`jira_mcp::JiraClient`] the broker's `jira_*`
//! tools use, at the same read+comment ceiling, and prints what the AGENT would see (the compact
//! rendering, not raw JSON).
//!
//! Configure it exactly like the broker would (all env, creds never on a command line):
//!
//! ```bash
//! export JIRA_URL=https://your-org.atlassian.net JIRA_USERNAME=you@example.com
//! export JIRA_API_TOKEN="$(cat ~/.jiratoken)"
//! cargo run -p crucible-broker --example jira_smoke -- 'project = PROJ ORDER BY created DESC'
//! # optional: one issue, then a comment to prove the write path
//! cargo run -p crucible-broker --example jira_smoke -- '' PROJ-1234 'hello from the broker'
//! ```

use jira_mcp::{Access, Config, JiraClient, render};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // arg1: JQL (optional). arg2 + arg3: an issue key + comment body (optional, exercises the write).
    let args: Vec<String> = std::env::args().skip(1).collect();
    let jql = args.first().cloned().unwrap_or_default();
    let key = args.get(1).cloned();
    let comment = args.get(2).cloned();

    let mut cfg = Config::from_env()
        .ok_or_else(|| anyhow::anyhow!("set JIRA_URL + JIRA_USERNAME + JIRA_API_TOKEN"))?;
    // The same ceiling the broker pins, so this smoke test cannot do something the broker couldn't.
    cfg.access = Access::ReadComment;
    let jira = JiraClient::new(cfg);

    if !jql.is_empty() {
        println!("== search: {jql}");
        println!("{}", render::search_rows(&jira.search(&jql, 5).await?));
    }
    if let Some(k) = &key {
        println!("\n== get_issue: {k}");
        let issue = jira.get_issue(k, false).await?;
        println!("{}", render::issue(&issue, jira.config(), 12_000));
    }
    if let (Some(k), Some(c)) = (&key, &comment) {
        println!("\n== add_comment: {k}");
        let id = jira.add_comment(k, c).await?;
        println!("id {id} at {}", jira.browse_url(k));
    }
    Ok(())
}

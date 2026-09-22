//! The co-draft handoff: what a local agent has to know to author a draft pack against THIS
//! controller, and the setup a human runs once to point that agent here.
//!
//! Both the served `SKILL.md` and the drafts-page hint are rendered from the same
//! [`CoDraft`], so the URL an agent reads and the URL a human pastes can never disagree.

/// The deployment as the co-draft handoff describes it.
#[derive(Debug, Clone)]
pub struct CoDraft {
    base: String,
    mcp_url: String,
}

/// One step of the setup, as the hint lists it and the skill file repeats it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupStep {
    pub label: String,
    pub commands: Vec<String>,
}

/// The filename the download lands under, and the name the skill installs as.
pub const SKILL_NAME: &str = "crucible-co-draft";
pub const SKILL_FILENAME: &str = "crucible-co-draft-SKILL.md";

impl CoDraft {
    /// `base` is this controller's own external base URL; `mcp_url` is where its MCP surface
    /// answers, which is `base/mcp` unless the deployment serves the machine paths on their own
    /// hostname.
    pub fn new(base: &str, mcp_url: Option<&str>) -> Self {
        let base = base.trim_end_matches('/').to_string();
        let mcp_url = mcp_url
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| format!("{base}/mcp"));
        CoDraft { base, mcp_url }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn mcp_url(&self) -> &str {
        &self.mcp_url
    }

    /// Where the skill file is served from, absolute so a pasted command works off-host.
    pub fn skill_url(&self) -> String {
        format!("{}/api/playbooks/drafts/skill", self.base)
    }

    /// The studio deep link for one draft: what the agent hands back when it is done.
    pub fn studio_url(&self, draft_id: &str) -> String {
        format!("{}/playbooks/drafts/{draft_id}", self.base)
    }

    pub fn setup(&self) -> Vec<SetupStep> {
        vec![
            SetupStep {
                label: "Install the skill".to_string(),
                commands: vec![
                    format!("mkdir -p ~/.claude/skills/{SKILL_NAME}"),
                    format!(
                        "mv ~/Downloads/{SKILL_FILENAME} ~/.claude/skills/{SKILL_NAME}/SKILL.md"
                    ),
                ],
            },
            SetupStep {
                label: format!(
                    "Mint an API key at {}/settings and keep it in your shell",
                    self.base
                ),
                commands: vec!["export CONTROLLER_API_TOKEN=crk_...".to_string()],
            },
            SetupStep {
                label: "Point your agent at this controller".to_string(),
                commands: vec![format!(
                    "claude mcp add --transport http crucible {} \\\n  --header \"Authorization: Bearer $CONTROLLER_API_TOKEN\"",
                    self.mcp_url
                )],
            },
        ]
    }

    /// A prompt that gets the loop started, so the first thing a reader does is not invent one.
    pub fn example_prompt(&self) -> String {
        "Use the crucible-co-draft skill: author a playbook pack that sweeps our repos for \
         dependencies pinned to a yanked release and writes a REPORT.md. Iterate until the \
         compile is clean, then hand me the studio link."
            .to_string()
    }

    /// The skill file itself, with this deployment's URL baked into every command in it.
    pub fn skill_markdown(&self) -> String {
        let base = &self.base;
        let mut out = String::new();
        out.push_str(&format!(
            "---\nname: {SKILL_NAME}\ndescription: Author a crucible playbook pack in the draft \
             studio at {base}, over the crucible MCP tools. Use when asked to write, edit or fix a \
             playbook pack, a workflow.star, or a crucible draft.\nallowed-tools: Read, Glob, \
             Grep, mcp__crucible__crucible_draft_create, mcp__crucible__crucible_draft_files, \
             mcp__crucible__crucible_draft_save, mcp__crucible__crucible_whoami\n---\n\n"
        ));
        out.push_str(&format!(
            "# Co-drafting a playbook pack\n\n\
             You author against the crucible controller at <{base}>. A pack lives there as a \
             *draft*: an append-only stack of saves, each one compiled by the controller's pinned \
             engine, each one launchable. You never write pack files to local disk — the draft is \
             the working copy, and a human is watching it in the studio while you edit.\n\n"
        ));
        out.push_str(
            "## The three tools\n\n\
             - `crucible_draft_create(draft_id, description, template?)` — opens a draft on a \
             minimal skeleton, or on a registered pack's files when `template` names one. Returns \
             version 1 and its diagnostics. The id is a lowercase slug and it is permanent.\n\
             - `crucible_draft_files(draft_id, version?)` — the whole `{path: content}` map at one \
             save, plus that save's version and diagnostics. Omit `version` for the newest.\n\
             - `crucible_draft_save(draft_id, base_version, files)` — writes the next version. \
             `files` is the WHOLE pack: a path you leave out is a file you delete. The save \
             compiles, and the diagnostics come back with `file:line:col` anchors.\n\n",
        );
        out.push_str(
            "## base_version discipline\n\n\
             `base_version` is the version you edited from. A human editing in the studio, or a \
             second agent, can save between your read and your write; when that happens the \
             controller refuses your save and writes nothing, naming the version that overtook \
             you.\n\n\
             1. Read with `crucible_draft_files` immediately before every save. Use the version it \
             returns, never a version you remember from earlier in the session.\n\
             2. On a refusal: re-read at the version the refusal names, merge your edits onto \
             those files, and save again with that version as the base. The files you get back \
             may hold someone's deliberate edit — keep it.\n\
             3. Never blind-retry a refused save, and never bump `base_version` to make the \
             refusal go away. Both destroy the other writer's work.\n\n",
        );
        out.push_str(&format!(
            "## Iterating\n\n\
             A save that does not compile is still a save — that is the point. Read the \
             diagnostics off each save, fix the anchored lines, save again. Stop when the \
             diagnostics are empty.\n\n\
             ## Handing back\n\n\
             Finish by giving the human the studio deep link on a line of its own:\n\n\
             ```\n{}\n```\n\n\
             They open it, read your diff against the previous version, and launch or graduate \
             from there.\n\n",
            self.studio_url("<draft_id>")
        ));
        out.push_str(
            "## If a call 403s\n\n\
             Run `crucible_whoami`. It reports the identity the controller resolved and how you \
             authenticated. Authoring needs an operator or admin session; a read-only identity can \
             list drafts and never save one.\n",
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_deployment_url_reaches_every_rendered_command() {
        let co = CoDraft::new("https://crucible.example.com/", None);
        assert_eq!(co.base(), "https://crucible.example.com");
        assert_eq!(
            co.skill_url(),
            "https://crucible.example.com/api/playbooks/drafts/skill"
        );
        assert_eq!(
            co.studio_url("sweep"),
            "https://crucible.example.com/playbooks/drafts/sweep"
        );
        let skill = co.skill_markdown();
        assert!(skill.starts_with("---\nname: crucible-co-draft\n"));
        assert!(skill.contains("https://crucible.example.com/playbooks/drafts/<draft_id>"));
        assert!(skill.contains("crucible_draft_create"));
        assert!(skill.contains("base_version"));
        assert!(!skill.contains("localhost"));
    }

    /// The MCP surface takes only a minted key, so the setup mints one and points the client at
    /// wherever that surface answers: the base by default, the machine hostname when the
    /// deployment has one.
    #[test]
    fn the_setup_mints_a_key_and_points_the_client_at_the_mcp_surface() {
        let same_host = CoDraft::new("https://crucible.example.com/", None);
        let joined = same_host
            .setup()
            .into_iter()
            .flat_map(|step| step.commands)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("export CONTROLLER_API_TOKEN=crk_"),
            "{joined}"
        );
        assert!(
            joined.contains(
                "claude mcp add --transport http crucible https://crucible.example.com/mcp"
            ),
            "{joined}"
        );
        assert!(
            joined.contains("--header \"Authorization: Bearer $CONTROLLER_API_TOKEN\""),
            "{joined}"
        );
        assert!(
            same_host.setup()[1]
                .label
                .contains("https://crucible.example.com/settings"),
            "the key is minted on the browser route"
        );

        let machine_host = CoDraft::new(
            "https://crucible.example.com",
            Some("https://crucible-api.example.com/mcp/"),
        );
        let joined = machine_host
            .setup()
            .into_iter()
            .flat_map(|step| step.commands)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("crucible https://crucible-api.example.com/mcp \\"),
            "{joined}"
        );
        assert_eq!(
            machine_host.mcp_url(),
            "https://crucible-api.example.com/mcp"
        );
        assert_eq!(
            CoDraft::new("https://crucible.example.com", Some("  ")).mcp_url(),
            "https://crucible.example.com/mcp",
            "a blank machine URL is no machine URL"
        );
    }

    #[test]
    fn the_skill_installs_under_the_name_it_declares() {
        let co = CoDraft::new("http://127.0.0.1:8899", None);
        let install = &co.setup()[0].commands;
        assert!(install[1].ends_with("~/.claude/skills/crucible-co-draft/SKILL.md"));
        assert!(install[1].contains(SKILL_FILENAME));
    }
}

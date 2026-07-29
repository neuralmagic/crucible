//! Deployment-flavored tool descriptions. The `#[tool(description)]` attributes in
//! [`crate::server`] carry the generic wording; a deployment can flavor the domain-specific
//! phrases (component names, profiler backend, JIRA project guidance) through `BROKER_DESC_*` /
//! `BROKER_JIRA_PROJECTS` env on the loop pod. The broker core stays domain-agnostic: with no env
//! set, the rendered text is exactly the generic attribute text (test-enforced), so the public
//! default carries no deployment's vocabulary.

use rmcp::handler::server::router::tool::ToolRouter;
use std::borrow::Cow;

/// The description knobs a deployment may set. Values are short phrases, not whole descriptions;
/// the templates below own the wording so every deployment gets the same tool contract prose.
#[derive(Debug, Default, Clone)]
pub struct DescVars {
    /// The component the build/deploy/measure tools operate on (e.g. a router name).
    /// `BROKER_DESC_COMPONENT`.
    pub component: Option<String>,
    /// The workspace the codegen build tool derives from (an upstream project name).
    /// `BROKER_DESC_CODEGEN_COMPONENT`.
    pub codegen_component: Option<String>,
    /// The profiling backend label (e.g. a `<component>/pprof` pairing).
    /// `BROKER_DESC_PROFILE_BACKEND`.
    pub profile_backend: Option<String>,
    /// Why profiling beats the end-to-end gate on this rig, the clause after "instead of
    /// guessing". `BROKER_DESC_PROFILE_MOTIVATION`.
    pub profile_motivation: Option<String>,
    /// Example `target` values for a multi-component rig. `BROKER_DESC_PROFILE_TARGET_EXAMPLE`.
    pub profile_target_example: Option<String>,
    /// The JIRA projects the search guidance names, in the order to list them.
    /// `BROKER_JIRA_PROJECTS` (comma/whitespace separated).
    pub jira_projects: Vec<String>,
    /// Example JQL for `jira_search`. `BROKER_DESC_JIRA_JQL_EXAMPLE`.
    pub jira_jql_example: Option<String>,
    /// Example issue key for `jira_get_issue`. `BROKER_DESC_JIRA_KEY_EXAMPLE`.
    pub jira_key_example: Option<String>,
}

impl DescVars {
    pub fn from_env() -> Self {
        Self {
            component: env_nonempty("BROKER_DESC_COMPONENT"),
            codegen_component: env_nonempty("BROKER_DESC_CODEGEN_COMPONENT"),
            profile_backend: env_nonempty("BROKER_DESC_PROFILE_BACKEND"),
            profile_motivation: env_nonempty("BROKER_DESC_PROFILE_MOTIVATION"),
            profile_target_example: env_nonempty("BROKER_DESC_PROFILE_TARGET_EXAMPLE"),
            jira_projects: env_nonempty("BROKER_JIRA_PROJECTS")
                .map(|s| {
                    s.split(|c: char| c == ',' || c.is_whitespace())
                        .filter(|p| !p.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            jira_jql_example: env_nonempty("BROKER_DESC_JIRA_JQL_EXAMPLE"),
            jira_key_example: env_nonempty("BROKER_DESC_JIRA_KEY_EXAMPLE"),
        }
    }

    /// The rendered (tool name, description) pairs for every parameterized tool.
    pub fn rendered(&self) -> Vec<(&'static str, String)> {
        vec![
            ("build_epp", self.build_epp()),
            ("deploy_candidate", self.deploy_candidate()),
            ("measure", self.measure()),
            ("profile", self.profile()),
            ("profile_query", self.profile_query()),
            ("codegen_build", self.codegen_build()),
            ("jira_search", self.jira_search()),
            ("jira_get_issue", self.jira_get_issue()),
        ]
    }

    fn build_epp(&self) -> String {
        let opening = match &self.component {
            Some(c) => format!("Build the {c} from your CURRENT edits into a candidate image."),
            None => "Build your CURRENT edits into a candidate image.".to_string(),
        };
        format!(
            "{opening} The loop pod pulls your working tree out of the sandbox, runs the \
             container build, and pushes it — you never hold build creds. Returns a JSON status: \
             built{{image_ref}} (ready to deploy) | compile_error{{log}} (fix the errors and call \
             again) | wrap_up{{reason}} (your candidate budget for this turn is spent — commit \
             your best CANDIDATE.md and END the turn) | disabled | error. Edit, build, fix, \
             repeat until it builds, then deploy_candidate."
        )
    }

    fn deploy_candidate(&self) -> String {
        let rig = match &self.component {
            Some(c) => format!("the live {c} rig"),
            None => "the live rig".to_string(),
        };
        format!(
            "Roll {rig} onto a built candidate so the judge measures it. Pass image_ref from a \
             build_epp `built` result, or omit it to deploy the latest candidate. Returns a JSON \
             status: deployed{{image_ref}} | disabled | error."
        )
    }

    fn measure(&self) -> String {
        let subject = self.component.as_deref().unwrap_or("candidate");
        format!(
            "Measure the currently-deployed {subject}'s fitness through the JUDGE's exact path. \
             The loop pod runs the gate's measurement engine-side (in-cluster, straight to the \
             rig — NOT the port-forward tunnel you'd be stuck with), so the number you get back \
             is the SAME one the judge gates on. Returns the judge's JSON record ({{valid, \
             score, detail{{...}}}}). Use this after deploy_candidate instead of running bench \
             yourself — it removes the tunnel confound entirely."
        )
    }

    fn backend(&self) -> String {
        match &self.profile_backend {
            Some(b) => format!("the {b} backend"),
            None => "a pprof backend".to_string(),
        }
    }

    fn profile(&self) -> String {
        let motivation = self.profile_motivation.as_deref().unwrap_or(
            "(an end-to-end gate can't isolate one component's hot path; a profile can)",
        );
        let backend = self.backend();
        let target = match &self.profile_target_example {
            Some(t) => format!("pass `target` (e.g. {t})"),
            None => "pass `target`".to_string(),
        };
        format!(
            "Capture a PROFILE of the deployed candidate's internals so you optimize the \
             MEASURED hot path instead of guessing {motivation}. The loop pod captures it (it \
             has the cluster reach + token + tooling you don't) and returns a handle. kind is \
             backend-defined; for {backend}: `profile` (CPU, the usual one — capture it WHILE a \
             `measure` is in flight for a hot profile), `heap`, `allocs`, `mutex`, `block`. Then \
             read it with `profile_query`. On a multi-component rig {target} to pick what to \
             profile; omit it when there's only one. Returns JSON: captured{{handle, target, \
             bytes}} | disabled | error."
        )
    }

    fn profile_query(&self) -> String {
        let backend = self.backend();
        format!(
            "Query a captured profile (read-only, text). Pass the kind of read you want; for \
             {backend}: `top` (hottest functions), `list=<FuncRegex>` (per-line ns/alloc in the \
             REAL source — this is the one that tells you exactly which lines to change), \
             `peek=<Func>`, `traces`, `tree`. Omit handle to query your most recent `profile` \
             capture. On a multi-component rig pass the same `target` you captured under. \
             Server/file-writing queries are refused. Use this to FIND what to change, then \
             build → deploy → `measure` to SCORE it. Returns JSON: query{{output}} | disabled | \
             error."
        )
    }

    fn codegen_build(&self) -> String {
        let workspace = match &self.codegen_component {
            Some(c) => format!("{c} workspace"),
            None => "workspace".to_string(),
        };
        format!(
            "Build your CURRENT {workspace} edits into a runnable, digest-pinned candidate \
             image. The loop pod pulls your working tree out of the sandbox and derives the \
             image (you never hold build creds). Pass mode=`derive` (fast, default) or `full` \
             (kernel compile) — only modes the scenario declares are accepted. Returns JSON: \
             built{{tree_hash, digest, mode, cached, log}} (pass digest to \
             codegen_benchmark/codegen_lm_eval; an identical tree replays the digest with \
             cached=true; `log` is the build-log handle) | job_failed{{reason, logs}} (build \
             error — fetch_log the handle, fix, rebuild) | rejected_kwarg | disabled | error. \
             The build log streams into its handle AS THE BUILD RUNS, so a concurrent session \
             can fetch_log it to watch progress."
        )
    }

    fn jira_search(&self) -> String {
        let jql = self
            .jira_jql_example
            .as_deref()
            .unwrap_or("project = PROJ AND labels = my-team");
        let scope = match prose_list(&self.jira_projects) {
            Some(list) => format!(
                "When gathering context for a goal, the projects of interest are {list} — scope \
                 your JQL to these rather than the whole instance. "
            ),
            None => String::new(),
        };
        let (noun, example_project) = match self.jira_projects.first() {
            Some(first) => ("These RFEs are", first.as_str()),
            None => ("Issues here may be", "internal"),
        };
        format!(
            "Search JIRA by JQL (read-only, mediated — the broker holds the credentials, you \
             never see a token). Pass `jql` (e.g. `{jql}`) and an optional `limit`. Returns \
             COMPACT rows {{key, summary, type, status, labels}} — call jira_get_issue for \
             detail. {scope}Returns {{\"status\":\"disabled\"}} when JIRA isn't configured. \
             {noun} AUTHORITATIVE for feature requirements but INTERNAL: reason about them \
             freely in your thinking and notes, but NEVER reference the internal SOURCE in \
             anything public — not the project key, epic name, ticket key, title, or a quote — \
             in code comments, commit messages, PR/issue bodies, or summaries. Don't even write \
             'the {example_project} epic requires X'; state X as an engineering requirement on \
             its own merits. Public artifacts attribute only public sources (upstream RFCs/PRs, \
             the code)."
        )
    }

    fn jira_get_issue(&self) -> String {
        let key = self.jira_key_example.as_deref().unwrap_or("PROJ-1234");
        format!(
            "Fetch one JIRA issue by key (read-only, mediated). Pass `issue_key` (e.g. `{key}`). \
             Returns a CURATED record by default — summary/status/type/priority/labels/ \
             components/description plus the custom fields that matter (intelligence_requested, \
             target_version, team, epic_link, product_manager, RICE/reach/confidence/effort, \
             release_type, sfdc_cases_links, blocked), with null fields omitted. Set `raw=true` \
             for the full untouched issue when you need a field the curated view drops. \
             {{\"status\":\"disabled\"}} when JIRA isn't configured. The issue is AUTHORITATIVE \
             for requirements but INTERNAL — reason about it freely, but never name the internal \
             source (project/epic/ticket key, title, quote) in public artifacts (code comments, \
             commit messages, PR/issue bodies, summaries); state requirements on their own \
             merits, attributing only public sources."
        )
    }
}

/// Overwrite the routed tools' descriptions with the rendered ones. Applied once at server
/// construction, so `tools/list` and `get_tool` both serve the flavored text.
pub(crate) fn apply<S>(router: &mut ToolRouter<S>, vars: &DescVars) {
    for (name, desc) in vars.rendered() {
        if let Some(route) = router.map.get_mut(name) {
            route.attr.description = Some(Cow::Owned(desc));
        }
    }
}

fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `a`, `a and b`, or `a, b, and c` (the serial-comma list the guidance sentence wants).
fn prose_list(items: &[String]) -> Option<String> {
    match items {
        [] => None,
        [one] => Some(one.clone()),
        [a, b] => Some(format!("{a} and {b}")),
        [head @ .., last] => Some(format!("{}, and {last}", head.join(", "))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generic default render must be byte-identical to the `#[tool(description)]` attribute
    /// text, so the source stays readable AND the override path can't drift from it.
    #[test]
    fn default_render_matches_the_attribute_text() {
        let router = crate::server::McpServer::tool_router();
        for (name, rendered) in DescVars::default().rendered() {
            let attr = router
                .get(name)
                .and_then(|t| t.description.clone())
                .unwrap_or_else(|| panic!("tool {name} missing from the router"));
            assert_eq!(
                attr.as_ref(),
                rendered,
                "tool {name}: attribute text and default render drifted"
            );
        }
    }

    /// The default render carries no deployment vocabulary at all.
    #[test]
    fn default_render_is_generic() {
        for (name, rendered) in DescVars::default().rendered() {
            for marker in ["EPP", "vLLM", "c=150", "TTFT"] {
                assert!(
                    !rendered.contains(marker),
                    "tool {name}: default description leaks {marker:?}"
                );
            }
        }
    }

    #[test]
    fn prose_list_uses_the_serial_comma() {
        let items = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(prose_list(&items(&[])), None);
        assert_eq!(prose_list(&items(&["A"])).as_deref(), Some("A"));
        assert_eq!(prose_list(&items(&["A", "B"])).as_deref(), Some("A and B"));
        assert_eq!(
            prose_list(&items(&["A", "B", "C"])).as_deref(),
            Some("A, B, and C")
        );
    }

    /// A flavored render lands the phrases where the templates put them (the exact internal
    /// deployment strings are byte-pinned by a test that never ships in the public export).
    #[test]
    fn flavored_render_places_the_phrases() {
        let vars = DescVars {
            component: Some("Router".into()),
            codegen_component: Some("Upstream".into()),
            profile_backend: Some("Router/pprof".into()),
            profile_motivation: Some("in the noise (the gate can't isolate it)".into()),
            profile_target_example: Some("`alpha` or `beta`".into()),
            jira_projects: vec!["AAA".into(), "BBB".into()],
            jira_jql_example: Some("project = AAA AND labels = x".into()),
            jira_key_example: Some("AAA-42".into()),
        };
        let get = |name: &str| {
            vars.rendered()
                .into_iter()
                .find(|(n, _)| *n == name)
                .map(|(_, d)| d)
                .unwrap_or_else(|| panic!("no render for {name}"))
        };
        assert!(get("build_epp").starts_with("Build the Router from your CURRENT edits"));
        assert!(get("deploy_candidate").starts_with("Roll the live Router rig onto"));
        assert!(get("measure").contains("currently-deployed Router's fitness"));
        assert!(get("profile").contains("guessing in the noise (the gate can't isolate it)."));
        assert!(get("profile").contains("for the Router/pprof backend:"));
        assert!(get("profile").contains("pass `target` (e.g. `alpha` or `beta`) to pick"));
        assert!(get("profile_query").contains("for the Router/pprof backend:"));
        assert!(get("codegen_build").starts_with("Build your CURRENT Upstream workspace edits"));
        let search = get("jira_search");
        assert!(search.contains("(e.g. `project = AAA AND labels = x`)"));
        assert!(search.contains("the projects of interest are AAA and BBB — scope"));
        assert!(search.contains("'the AAA epic requires X'"));
        assert!(get("jira_get_issue").contains("(e.g. `AAA-42`)"));
    }
}

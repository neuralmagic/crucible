//! The policy engine of ADR-0039: a Cedar policy set validated against a schema rendered from
//! the action vocabulary, identified by a content digest, swapped at runtime by `activate`.

use crate::authz::action::{Action, ResourceType, Verb};
use crate::authz::decision::{Decision, Resource, Subject};
use crate::authz::model::{Principal, TeamRole, TeamSlug};
use cedar_policy::{
    Authorizer, Context, Entities, Entity, EntityUid, PolicyId, PolicySet, Request,
    RestrictedExpression, Schema, ValidationMode, Validator,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

/// The default policy set shipped with the controller (RFC-0003 C-POLICY).
pub const DEFAULT_POLICY: &str = include_str!("default.cedar");

/// The version of the rendered schema: bumped when the vocabulary or the entity shapes change so
/// a stored set can be told apart from one written against an older schema.
pub const SCHEMA_VERSION: i32 = 1;

/// Why a policy set was refused (RFC-0003 C-POLICY: refused with the failing rule named).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("the policy set does not parse: {0}")]
    Parse(String),
    #[error("the policy schema does not build: {0}")]
    Schema(String),
    #[error("policy {policy} fails validation: {detail}")]
    Invalid { policy: String, detail: String },
    #[error("policy {0} carries no @id annotation")]
    MissingId(String),
    #[error("two policies carry @id {0}")]
    DuplicateId(String),
    #[error("{0}")]
    Probe(String),
}

/// A validated policy set and the schema it was validated against.
pub struct Engine {
    schema: Schema,
    policies: PolicySet,
    ids: HashMap<PolicyId, String>,
    digest: String,
    authorizer: Authorizer,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("digest", &self.digest)
            .field("policies", &self.ids.len())
            .finish()
    }
}

/// The content digest of a policy set: SHA-256 of the text with `\r\n` folded to `\n` and
/// trailing whitespace stripped per line.
pub fn digest(text: &str) -> String {
    let canonical: String = text
        .replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    format!("{:x}", Sha256::digest(canonical.as_bytes()))
}

/// The Cedar schema over the vocabulary: principal types, one entity type per resource type, a
/// group action per verb, and one action per `<resource>:<verb>`.
pub fn render_schema() -> String {
    let mut out = String::new();
    for name in ["UserPrincipal", "TeamPrincipal", "RunPrincipal"] {
        out.push_str(&format!(
            "entity {name} {{ id: String, platform_admin: Bool, proves_groups: Bool }} tags String;\n"
        ));
    }
    for resource in ResourceType::ALL {
        out.push_str(&format!(
            "entity {} {{ id: String, owner: String, owner_kind: String, owner_role?: String, share?: String, run?: String }};\n",
            resource.entity_type()
        ));
    }
    let mut verbs: Vec<Verb> = ResourceType::ALL.iter().flat_map(|r| r.verbs()).collect();
    verbs.sort();
    verbs.dedup();
    for verb in &verbs {
        out.push_str(&format!("action \"{}\";\n", verb.as_str()));
    }
    for action in Action::all() {
        out.push_str(&format!(
            "action \"{action}\" in [Action::\"{}\"] appliesTo {{ principal: [UserPrincipal, TeamPrincipal, RunPrincipal], resource: [{}], context: {{ now: Long }} }};\n",
            action.verb.as_str(),
            action.resource.entity_type()
        ));
    }
    out
}

fn schema() -> Result<Schema, PolicyError> {
    let (schema, _warnings) = Schema::from_cedarschema_str(&render_schema())
        .map_err(|e| PolicyError::Schema(e.to_string()))?;
    Ok(schema)
}

impl Engine {
    /// Parse, validate, and probe a policy set.
    pub fn load(text: &str) -> Result<Engine, PolicyError> {
        let schema = schema()?;
        let policies: PolicySet = text
            .parse()
            .map_err(|e: cedar_policy::ParseErrors| PolicyError::Parse(e.to_string()))?;
        let mut ids = HashMap::new();
        let mut seen = BTreeMap::new();
        for policy in policies.policies() {
            let Some(id) = policy.annotation("id") else {
                return Err(PolicyError::MissingId(policy.id().to_string()));
            };
            if seen.insert(id.to_string(), ()).is_some() {
                return Err(PolicyError::DuplicateId(id.to_string()));
            }
            ids.insert(policy.id().clone(), id.to_string());
        }
        let result = Validator::new(schema.clone()).validate(&policies, ValidationMode::Strict);
        if let Some(error) = result.validation_errors().next() {
            let policy = ids
                .get(error.policy_id())
                .cloned()
                .unwrap_or_else(|| error.policy_id().to_string());
            return Err(PolicyError::Invalid {
                policy,
                detail: error.to_string(),
            });
        }
        let engine = Engine {
            schema,
            policies,
            ids,
            digest: digest(text),
            authorizer: Authorizer::new(),
        };
        engine.probe()?;
        Ok(engine)
    }

    /// The structural checks of RFC-0003 C-POLICY: no role below owner holds `manage-members` or
    /// `transfer` on a team, and the strongest principal still holds `activate`.
    fn probe(&self) -> Result<(), PolicyError> {
        let probe = TeamSlug::parse("probe").map_err(|e| PolicyError::Probe(e.to_string()))?;
        let team = Resource::new(ResourceType::Team, "probe", Principal::Team(probe.clone()));
        for role in [TeamRole::Member, TeamRole::Maintainer] {
            let subject = Subject::probe_member(&probe, role);
            for verb in [Verb::ManageMembers, Verb::Transfer] {
                let action = Action {
                    resource: ResourceType::Team,
                    verb,
                };
                let decision = self.authorize(&subject, action, &team, 0);
                if decision.allowed {
                    return Err(PolicyError::Probe(format!(
                        "policy {} grants {action} to the {} role",
                        decision.reason(),
                        role.as_str()
                    )));
                }
            }
        }
        let set = Resource::new(
            ResourceType::PolicySet,
            "probe",
            Principal::Team(TeamSlug::platform_administrators()),
        );
        let action = Action {
            resource: ResourceType::PolicySet,
            verb: Verb::Activate,
        };
        if !self
            .authorize(&Subject::probe_platform_admin(), action, &set, 0)
            .allowed
        {
            return Err(PolicyError::Probe(
                "no user or team principal holds policy_set:activate".to_string(),
            ));
        }
        Ok(())
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The ids of every policy in the set, in source order.
    pub fn policy_ids(&self) -> Vec<String> {
        self.policies
            .policies()
            .filter_map(|p| self.ids.get(p.id()).cloned())
            .collect()
    }

    /// The one decision of RFC-0003 C-DECISION. `now` is epoch seconds.
    pub fn authorize(
        &self,
        subject: &Subject,
        action: Action,
        resource: &Resource,
        now: i64,
    ) -> Decision {
        match self.evaluate(subject, action, resource, now) {
            Ok(decision) => decision,
            Err(detail) => Decision::denied(format!("invalid-request: {detail}")),
        }
    }

    fn evaluate(
        &self,
        subject: &Subject,
        action: Action,
        resource: &Resource,
        now: i64,
    ) -> Result<Decision, String> {
        let principal_uid: EntityUid = format!(
            "{}::{}",
            subject.entity_type(),
            cedar_string(subject.principal.name())
        )
        .parse()
        .map_err(|e: cedar_policy::ParseErrors| e.to_string())?;
        let principal = Entity::new_with_tags(
            principal_uid.clone(),
            [
                (
                    "id".to_string(),
                    RestrictedExpression::new_string(subject.principal.to_string()),
                ),
                (
                    "platform_admin".to_string(),
                    RestrictedExpression::new_bool(subject.platform_admin),
                ),
                (
                    "proves_groups".to_string(),
                    RestrictedExpression::new_bool(subject.proves_groups),
                ),
            ],
            [],
            subject
                .tags
                .iter()
                .map(|(k, v)| (k.clone(), RestrictedExpression::new_string(v.clone()))),
        )
        .map_err(|e| e.to_string())?;

        let resource_uid: EntityUid = format!(
            "{}::{}",
            resource.rtype.entity_type(),
            cedar_string(&resource.id)
        )
        .parse()
        .map_err(|e: cedar_policy::ParseErrors| e.to_string())?;
        let mut attrs = vec![
            (
                "id".to_string(),
                RestrictedExpression::new_string(resource.id.clone()),
            ),
            (
                "owner".to_string(),
                RestrictedExpression::new_string(resource.owner.to_string()),
            ),
            (
                "owner_kind".to_string(),
                RestrictedExpression::new_string(resource.owner_kind().to_string()),
            ),
        ];
        if let Some(role) = resource.owner_role(subject) {
            attrs.push((
                "owner_role".to_string(),
                RestrictedExpression::new_string(role.as_str().to_string()),
            ));
        }
        if let Some(share) = resource.share {
            attrs.push((
                "share".to_string(),
                RestrictedExpression::new_string(share.as_str().to_string()),
            ));
        }
        if let Some(run) = &resource.run {
            attrs.push((
                "run".to_string(),
                RestrictedExpression::new_string(Principal::Run(run.clone()).to_string()),
            ));
        }
        let resource_entity =
            Entity::new(resource_uid.clone(), attrs.into_iter().collect(), [].into())
                .map_err(|e| e.to_string())?;

        let action_uid: EntityUid = format!("Action::{}", cedar_string(&action.to_string()))
            .parse()
            .map_err(|e: cedar_policy::ParseErrors| e.to_string())?;
        let context =
            Context::from_pairs([("now".to_string(), RestrictedExpression::new_long(now))])
                .map_err(|e| e.to_string())?;
        let request = Request::new(
            principal_uid,
            action_uid,
            resource_uid,
            context,
            Some(&self.schema),
        )
        .map_err(|e| e.to_string())?;
        let entities = Entities::from_entities([principal, resource_entity], Some(&self.schema))
            .map_err(|e| e.to_string())?;
        let response = self
            .authorizer
            .is_authorized(&request, &self.policies, &entities);
        let mut rules: Vec<String> = response
            .diagnostics()
            .reason()
            .map(|id| self.ids.get(id).cloned().unwrap_or_else(|| id.to_string()))
            .collect();
        rules.sort();
        Ok(Decision {
            allowed: response.decision() == cedar_policy::Decision::Allow,
            rules,
        })
    }
}

/// A Cedar string literal: quoted, with backslashes and quotes escaped.
fn cedar_string(raw: &str) -> String {
    format!("\"{}\"", raw.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The engine in force, swapped whole by `activate`.
#[derive(Clone)]
pub struct ActivePolicy(Arc<RwLock<Arc<Engine>>>);

impl std::fmt::Debug for ActivePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ActivePolicy")
            .field(&self.current().digest)
            .finish()
    }
}

impl ActivePolicy {
    pub fn new(engine: Engine) -> Self {
        ActivePolicy(Arc::new(RwLock::new(Arc::new(engine))))
    }

    /// The shipped default, which a test proves loads.
    pub fn default_set() -> Result<Self, PolicyError> {
        Engine::load(DEFAULT_POLICY).map(ActivePolicy::new)
    }

    pub fn current(&self) -> Arc<Engine> {
        match self.0.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn swap(&self, engine: Engine) {
        let engine = Arc::new(engine);
        match self.0.write() {
            Ok(mut guard) => *guard = engine,
            Err(poisoned) => *poisoned.into_inner() = engine,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::decision::ShareRole;
    use crate::authz::model::PLATFORM_ADMINISTRATORS;

    fn slug(s: &str) -> TeamSlug {
        TeamSlug::parse(s).expect("slug")
    }

    fn user(login: &str) -> Subject {
        Subject::user(login, false, true, BTreeMap::new())
    }

    fn act(resource: ResourceType, verb: Verb) -> Action {
        Action { resource, verb }
    }

    #[test]
    fn the_default_set_loads_and_names_its_policies() {
        let engine = Engine::load(DEFAULT_POLICY).expect("default policy set");
        assert_eq!(engine.digest(), digest(DEFAULT_POLICY));
        let ids = engine.policy_ids();
        assert!(ids.contains(&"platform-admin-all".to_string()));
        assert!(ids.contains(&"team-owner-all".to_string()));
        assert!(ids.contains(&"everyone-read-platform-providers".to_string()));
        assert_eq!(ids.len(), 16);
    }

    #[test]
    fn the_digest_ignores_line_endings_and_trailing_whitespace() {
        assert_eq!(digest("a  \r\nb\n"), digest("a\nb"));
        assert_ne!(digest("a\nb"), digest("a\nc"));
    }

    #[test]
    fn a_set_without_ids_or_with_a_duplicate_is_refused() {
        let err = Engine::load("permit(principal, action, resource);").expect_err("no id");
        assert!(matches!(err, PolicyError::MissingId(_)), "{err}");
        let err = Engine::load(
            "@id(\"x\") permit(principal, action, resource);\n@id(\"x\") permit(principal, action, resource);",
        )
        .expect_err("dup");
        assert_eq!(err, PolicyError::DuplicateId("x".into()));
        let err = Engine::load(
            "@id(\"x\") permit(principal, action, resource) when { resource.nope == 1 };",
        )
        .expect_err("invalid attr");
        assert!(matches!(err, PolicyError::Invalid { .. }), "{err}");
        let err = Engine::load("@id(\"x\") permit(principal, action, resource").expect_err("parse");
        assert!(matches!(err, PolicyError::Parse(_)), "{err}");
    }

    #[test]
    fn the_probes_refuse_a_lockout_and_a_manage_members_grant_below_owner() {
        let widened = format!(
            "{DEFAULT_POLICY}\n@id(\"bad\") permit(principal, action == Action::\"team:manage-members\", resource) when {{ resource has owner_role }};"
        );
        let err = Engine::load(&widened).expect_err("widened");
        assert!(
            matches!(&err, PolicyError::Probe(msg) if msg.contains("bad") && msg.contains("member")),
            "{err}"
        );
        let err = Engine::load(
            "@id(\"only-read\") permit(principal, action in [Action::\"read\"], resource);",
        )
        .expect_err("lockout");
        assert!(
            matches!(&err, PolicyError::Probe(msg) if msg.contains("activate")),
            "{err}"
        );
    }

    #[test]
    fn a_user_owner_holds_every_action_and_nobody_else_does() {
        let engine = Engine::load(DEFAULT_POLICY).expect("default");
        let secret = Resource::new(ResourceType::Secret, "s1", Principal::User("alice".into()));
        for verb in ResourceType::Secret.verbs() {
            let d = engine.authorize(&user("alice"), act(ResourceType::Secret, verb), &secret, 0);
            assert!(d.allowed, "{verb:?}: {}", d.reason());
            assert_eq!(d.rules, vec!["user-owner-all"]);
            let d = engine.authorize(&user("bob"), act(ResourceType::Secret, verb), &secret, 0);
            assert!(!d.allowed, "{verb:?}");
            assert_eq!(d.reason(), "no-rule");
        }
    }

    #[test]
    fn team_roles_confer_what_the_rfc_lists() {
        let engine = Engine::load(DEFAULT_POLICY).expect("default");
        let playbook = Resource::new(ResourceType::Playbook, "p", Principal::Team(slug("llm-d")));
        let at = |role: &str| {
            Subject::user(
                "alice",
                false,
                true,
                BTreeMap::from([("team:llm-d".to_string(), role.to_string())]),
            )
        };
        let allowed = |role: &str, verb: Verb| {
            engine
                .authorize(&at(role), act(ResourceType::Playbook, verb), &playbook, 0)
                .allowed
        };
        assert!(allowed("member", Verb::Read));
        assert!(allowed("member", Verb::Launch));
        assert!(!allowed("member", Verb::Update));
        assert!(!allowed("member", Verb::Delete));
        assert!(allowed("maintainer", Verb::Update));
        assert!(allowed("maintainer", Verb::Create));
        assert!(!allowed("maintainer", Verb::Delete));
        assert!(!allowed("maintainer", Verb::Transfer));
        assert!(!allowed("maintainer", Verb::Share));
        assert!(allowed("owner", Verb::Delete));
        assert!(allowed("owner", Verb::Transfer));
        assert!(allowed("owner", Verb::Share));
        let stranger = engine.authorize(
            &user("zed"),
            act(ResourceType::Playbook, Verb::Read),
            &playbook,
            0,
        );
        assert!(!stranger.allowed);
    }

    #[test]
    fn platform_grants_read_and_dispatch_to_every_user_and_everything_to_admins() {
        let engine = Engine::load(DEFAULT_POLICY).expect("default");
        let admins = Principal::Team(slug(PLATFORM_ADMINISTRATORS));
        let platform = Resource::new(ResourceType::Platform, "platform", admins.clone());
        let target = Resource::new(ResourceType::DispatchTarget, "gpu", admins.clone());
        assert!(
            engine
                .authorize(
                    &user("zed"),
                    act(ResourceType::Platform, Verb::Read),
                    &platform,
                    0
                )
                .allowed
        );
        assert!(
            !engine
                .authorize(
                    &user("zed"),
                    act(ResourceType::Platform, Verb::Update),
                    &platform,
                    0
                )
                .allowed
        );
        assert!(
            engine
                .authorize(
                    &user("zed"),
                    act(ResourceType::DispatchTarget, Verb::Dispatch),
                    &target,
                    0
                )
                .allowed
        );
        let admin = Subject::probe_platform_admin();
        let d = engine.authorize(
            &admin,
            act(ResourceType::Platform, Verb::Update),
            &platform,
            0,
        );
        assert!(d.allowed);
        assert!(
            d.rules.contains(&"platform-admin-all".to_string()),
            "{}",
            d.reason()
        );
        let operator = Subject::user(
            "op",
            false,
            true,
            BTreeMap::from([("team:platform-operators".to_string(), "member".to_string())]),
        );
        let issue = Resource::new(ResourceType::Issue, "org/repo#1", admins);
        assert!(
            engine
                .authorize(&operator, act(ResourceType::Issue, Verb::Update), &issue, 0)
                .allowed
        );
        assert!(
            !engine
                .authorize(&operator, act(ResourceType::Issue, Verb::Launch), &issue, 0)
                .allowed
        );
        assert!(
            !engine
                .authorize(
                    &user("zed"),
                    act(ResourceType::Issue, Verb::Update),
                    &issue,
                    0
                )
                .allowed
        );
    }

    #[test]
    fn shares_confer_only_their_role_and_never_ownership_actions() {
        let engine = Engine::load(DEFAULT_POLICY).expect("default");
        let mut playbook =
            Resource::new(ResourceType::Playbook, "p", Principal::User("alice".into()));
        playbook.share = Some(ShareRole::Editor);
        let bob = user("bob");
        assert!(
            engine
                .authorize(&bob, act(ResourceType::Playbook, Verb::Read), &playbook, 0)
                .allowed
        );
        assert!(
            engine
                .authorize(
                    &bob,
                    act(ResourceType::Playbook, Verb::Launch),
                    &playbook,
                    0
                )
                .allowed
        );
        assert!(
            engine
                .authorize(
                    &bob,
                    act(ResourceType::Playbook, Verb::Update),
                    &playbook,
                    0
                )
                .allowed
        );
        assert!(
            !engine
                .authorize(
                    &bob,
                    act(ResourceType::Playbook, Verb::Delete),
                    &playbook,
                    0
                )
                .allowed
        );
        assert!(
            !engine
                .authorize(&bob, act(ResourceType::Playbook, Verb::Share), &playbook, 0)
                .allowed
        );
        playbook.share = Some(ShareRole::Viewer);
        assert!(
            engine
                .authorize(&bob, act(ResourceType::Playbook, Verb::Read), &playbook, 0)
                .allowed
        );
        assert!(
            !engine
                .authorize(
                    &bob,
                    act(ResourceType::Playbook, Verb::Launch),
                    &playbook,
                    0
                )
                .allowed
        );
    }

    #[test]
    fn a_run_principal_reads_its_tree_and_publishes_only_itself() {
        let engine = Engine::load(DEFAULT_POLICY).expect("default");
        let run = Subject::run("r1");
        let mut own = Resource::new(ResourceType::Run, "r1", Principal::User("alice".into()));
        own.run = Some("r1".into());
        let mut other = Resource::new(ResourceType::Run, "r2", Principal::User("alice".into()));
        other.run = Some("r2".into());
        let mut launch = Resource::new(
            ResourceType::StandingLaunch,
            "l1",
            Principal::User("alice".into()),
        );
        launch.run = Some("r1".into());
        assert!(
            engine
                .authorize(&run, act(ResourceType::Run, Verb::Read), &own, 0)
                .allowed
        );
        assert!(
            engine
                .authorize(&run, act(ResourceType::Run, Verb::Publish), &own, 0)
                .allowed
        );
        assert!(
            engine
                .authorize(
                    &run,
                    act(ResourceType::StandingLaunch, Verb::Read),
                    &launch,
                    0
                )
                .allowed
        );
        assert!(
            !engine
                .authorize(
                    &run,
                    act(ResourceType::StandingLaunch, Verb::Launch),
                    &launch,
                    0
                )
                .allowed
        );
        assert!(
            !engine
                .authorize(&run, act(ResourceType::Run, Verb::Read), &other, 0)
                .allowed
        );
        assert!(
            !engine
                .authorize(
                    &run,
                    act(ResourceType::Platform, Verb::Read),
                    &Resource::new(
                        ResourceType::Platform,
                        "platform",
                        Principal::Team(slug(PLATFORM_ADMINISTRATORS))
                    ),
                    0
                )
                .allowed
        );
    }

    #[test]
    fn an_expiring_rule_stops_at_the_controller_clock() {
        let text = format!(
            "{DEFAULT_POLICY}\n@id(\"temp\") permit(principal, action == Action::\"platform:update\", resource) when {{ principal.hasTag(\"user:zed\") && context.now < 100 }};"
        );
        let engine = Engine::load(&text).expect("loads");
        let zed = Subject::user(
            "zed",
            false,
            true,
            BTreeMap::from([("user:zed".to_string(), "owner".to_string())]),
        );
        let platform = Resource::new(
            ResourceType::Platform,
            "platform",
            Principal::Team(slug(PLATFORM_ADMINISTRATORS)),
        );
        let d = engine.authorize(
            &zed,
            act(ResourceType::Platform, Verb::Update),
            &platform,
            99,
        );
        assert!(d.allowed);
        assert_eq!(d.rules, vec!["temp"]);
        assert!(
            !engine
                .authorize(
                    &zed,
                    act(ResourceType::Platform, Verb::Update),
                    &platform,
                    100
                )
                .allowed
        );
    }

    #[test]
    fn a_swap_replaces_the_engine_in_force() {
        let active = ActivePolicy::default_set().expect("default");
        let before = active.current().digest().to_string();
        let text = format!(
            "{DEFAULT_POLICY}\n@id(\"extra\") permit(principal, action == Action::\"platform:read\", resource);"
        );
        active.swap(Engine::load(&text).expect("loads"));
        assert_ne!(active.current().digest(), before);
        assert!(active.current().policy_ids().contains(&"extra".to_string()));
    }
}

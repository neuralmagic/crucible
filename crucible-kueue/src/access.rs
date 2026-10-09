//! Every tool and method crucible-kueue serves, the scope each needs, and the Kubernetes access a
//! call needs when the caller carries a Kubernetes user (a person, or a run acting as its
//! launcher). The guard refuses anything not listed here.
//!
//! | Call                                  | Scope        | Access check                           |
//! |---------------------------------------|--------------|----------------------------------------|
//! | whoami                                | kueue.read   | none                                   |
//! | queues_list                           | kueue.read   | `list` localqueues                     |
//! | queue_describe, hardware_describe     | kueue.read   | `get` the localqueue                   |
//! | workload_status, tasks/get            | kueue.read   | `get` the workload                     |
//! | workload_list                         | kueue.read   | `list` kueue workloads                 |
//! | workload_logs                         | kueue.read   | `get` pods/log                         |
//! | workload_plan, workload_submit        | kueue.submit | `use` the localqueue, `create` the kind |
//! | workload_scale                        | kueue.submit | `update` the workload's scale          |
//! | workload_stop, workload_resume        | kueue.submit | `update` the workload                  |
//! | tasks/cancel                          | kueue.submit | `update` the workload                  |
//! | workload_delete                       | kueue.submit | `delete` the workload                  |
//!
//! The namespace checked is the call's `namespace` argument, else the token's `namespace` claim.

use crate::manifest::WorkloadKind;
use crate::task::TaskRef;
use crucible_contract::kueue::{SCOPE_READ, SCOPE_SUBMIT};
use crucible_mcp_auth::access::{AccessCheck, AccessRules, Call};
use crucible_mcp_auth::kube::USER_CLAIM;
use crucible_mcp_auth::policy::Refusal;
use crucible_mcp_auth::principal::Principal;
use serde_json::{Map, Value};

const KUEUE_GROUP: &str = "kueue.x-k8s.io";
const READ: &[&str] = &[SCOPE_READ];
const SUBMIT: &[&str] = &[SCOPE_SUBMIT];

pub struct KueueRules;

impl AccessRules for KueueRules {
    fn scopes(&self, call: &Call<'_>) -> Result<&'static [&'static str], Refusal> {
        match call {
            Call::Tool { name, .. } => match *name {
                "whoami" | "queues_list" | "queue_describe" | "hardware_describe"
                | "workload_status" | "workload_list" | "workload_logs" => Ok(READ),
                "workload_plan" | "workload_submit" | "workload_scale" | "workload_stop"
                | "workload_resume" | "workload_delete" => Ok(SUBMIT),
                _ => Err(call.unserved()),
            },
            Call::Method { name, .. } => match *name {
                "tasks/get" => Ok(READ),
                "tasks/cancel" => Ok(SUBMIT),
                _ => Err(call.unserved()),
            },
        }
    }

    fn checks(&self, call: &Call<'_>, principal: &Principal) -> Result<Vec<AccessCheck>, Refusal> {
        if !principal.claims.contains_key(USER_CLAIM) {
            return Ok(Vec::new());
        }
        let default_namespace = principal.namespace.as_deref().unwrap_or_default();
        match call {
            Call::Tool { name, args } => tool_checks(name, args, default_namespace),
            Call::Method { name, params } => method_checks(name, params, default_namespace),
        }
    }
}

struct Checks<'a> {
    call: &'a str,
    namespace: String,
}

impl Checks<'_> {
    fn check(
        &self,
        rule: &str,
        message: &str,
        target: (&str, &str, &str),
        verb: &str,
        name: &str,
    ) -> AccessCheck {
        let (group, resource, subresource) = target;
        AccessCheck {
            tool: self.call.to_string(),
            rule: rule.to_string(),
            message: message.to_string(),
            group: group.to_string(),
            resource: resource.to_string(),
            subresource: subresource.to_string(),
            verb: verb.to_string(),
            namespace: self.namespace.clone(),
            name: name.to_string(),
        }
    }

    fn workload(
        &self,
        rule: &str,
        message: &str,
        kind: WorkloadKind,
        verb: &str,
        name: &str,
    ) -> AccessCheck {
        self.check(rule, message, (group(kind), kind.plural(), ""), verb, name)
    }
}

fn group(kind: WorkloadKind) -> &'static str {
    kind.api_version()
        .rsplit_once('/')
        .map_or("", |(group, _)| group)
}

fn text<'a>(args: &'a Map<String, Value>, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// The workload kind a call names, or a refusal naming the check that could not be resolved.
fn kind(call: &str, rule: &str, value: Option<&Value>) -> Result<WorkloadKind, Refusal> {
    let named = value.and_then(Value::as_str).unwrap_or_default();
    WorkloadKind::ALL
        .into_iter()
        .find(|k| k.kind() == named)
        .ok_or_else(|| Refusal::ReviewError {
            tool: call.to_string(),
            rule: rule.to_string(),
            error: format!("{named:?} is not a workload kind this server manages"),
        })
}

fn tool_checks(
    tool: &str,
    args: &Map<String, Value>,
    default_namespace: &str,
) -> Result<Vec<AccessCheck>, Refusal> {
    let namespace = match text(args, "namespace") {
        "" => default_namespace,
        named => named,
    };
    let c = Checks {
        call: tool,
        namespace: namespace.to_string(),
    };
    let queues = (KUEUE_GROUP, "localqueues", "");
    let name = text(args, "name");
    Ok(match tool {
        "whoami" => Vec::new(),
        "queues_list" => vec![c.check(
            "queues-list",
            "you may not list LocalQueues in this namespace",
            queues,
            "list",
            "",
        )],
        "queue_describe" | "hardware_describe" => vec![c.check(
            "queue-get",
            "you may not read this LocalQueue",
            queues,
            "get",
            text(args, "queue"),
        )],
        "workload_status" => {
            let kind = kind(tool, "workload-get", args.get("kind"))?;
            vec![c.workload(
                "workload-get",
                "you may not read this workload",
                kind,
                "get",
                name,
            )]
        }
        "workload_list" => vec![c.check(
            "workloads-list",
            "you may not list Kueue workloads in this namespace",
            (KUEUE_GROUP, "workloads", ""),
            "list",
            "",
        )],
        "workload_logs" => vec![c.check(
            "pod-logs",
            "you may not read pod logs in this namespace",
            ("", "pods", "log"),
            "get",
            "",
        )],
        "workload_plan" | "workload_submit" => {
            let kind = kind(
                tool,
                "workload-create",
                args.get("manifest").and_then(|m| m.get("kind")),
            )?;
            vec![
                c.check(
                    "queue-use",
                    "you may not submit to this queue",
                    queues,
                    "use",
                    text(args, "queue"),
                ),
                c.workload(
                    "workload-create",
                    "you may not create this kind of workload in this namespace",
                    kind,
                    "create",
                    "",
                ),
            ]
        }
        "workload_scale" => {
            let kind = kind(tool, "workload-scale", args.get("kind"))?;
            vec![c.check(
                "workload-scale",
                "you may not scale this workload",
                (group(kind), kind.plural(), "scale"),
                "update",
                name,
            )]
        }
        "workload_stop" => {
            let kind = kind(tool, "workload-update", args.get("kind"))?;
            vec![c.workload(
                "workload-update",
                "you may not stop this workload",
                kind,
                "update",
                name,
            )]
        }
        "workload_resume" => {
            let kind = kind(tool, "workload-update", args.get("kind"))?;
            vec![c.workload(
                "workload-update",
                "you may not resume this workload",
                kind,
                "update",
                name,
            )]
        }
        "workload_delete" => {
            let kind = kind(tool, "workload-delete", args.get("kind"))?;
            vec![c.workload(
                "workload-delete",
                "you may not delete this workload",
                kind,
                "delete",
                name,
            )]
        }
        _ => {
            return Err(Refusal::Unlisted {
                tool: tool.to_string(),
            });
        }
    })
}

fn method_checks(
    method: &str,
    params: &Value,
    default_namespace: &str,
) -> Result<Vec<AccessCheck>, Refusal> {
    let (rule, message, verb) = match method {
        "tasks/get" => ("task-get", "you may not read this workload", "get"),
        "tasks/cancel" => ("task-cancel", "you may not cancel this workload", "update"),
        _ => {
            return Err(Refusal::Method {
                method: method.to_string(),
            });
        }
    };
    let id = params
        .get("taskId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(task) = TaskRef::parse(id) else {
        return Err(Refusal::ReviewError {
            tool: method.to_string(),
            rule: rule.to_string(),
            error: format!("unknown task: {id}"),
        });
    };
    let c = Checks {
        call: method,
        namespace: task
            .namespace
            .clone()
            .unwrap_or_else(|| default_namespace.to_string()),
    };
    Ok(vec![c.workload(rule, message, task.kind, verb, &task.name)])
}

#[cfg(test)]
mod tests {
    use crate::access::*;
    use crucible_mcp_auth::principal::Callers;
    use serde_json::json;

    fn principal(claims: Value) -> Principal {
        match claims {
            Value::Object(m) => Principal::from_claims(m, Callers::People).unwrap(),
            other => panic!("{other}"),
        }
    }

    fn person() -> Principal {
        principal(json!({"sub": "p", "kube_user": "alice", "scope": "kueue.read kueue.submit"}))
    }

    fn tool_checks_for(
        p: &Principal,
        name: &str,
        args: Value,
    ) -> Result<Vec<AccessCheck>, Refusal> {
        let Value::Object(args) = args else {
            panic!("args must be an object")
        };
        KueueRules.checks(&Call::Tool { name, args: &args }, p)
    }

    fn shown(checks: Vec<AccessCheck>) -> Vec<String> {
        checks
            .into_iter()
            .map(|c| {
                format!(
                    "{} {}/{}/{} {} ns={} name={}",
                    c.rule, c.group, c.resource, c.subresource, c.verb, c.namespace, c.name
                )
            })
            .collect()
    }

    #[test]
    fn unknown_tools_and_methods_are_refused() {
        let args = Map::new();
        assert_eq!(
            KueueRules.scopes(&Call::Tool {
                name: "rm_rf",
                args: &args
            }),
            Err(Refusal::Unlisted {
                tool: "rm_rf".into()
            })
        );
        assert_eq!(
            KueueRules.scopes(&Call::Method {
                name: "tasks/list",
                params: &Value::Null
            }),
            Err(Refusal::Method {
                method: "tasks/list".into()
            })
        );
    }

    #[test]
    fn submitting_checks_the_queue_and_the_kind_in_the_named_namespace() {
        let checks = tool_checks_for(
            &person(),
            "workload_submit",
            json!({"namespace": "team-b", "queue": "gpu", "manifest": {"kind": "RayJob"}}),
        )
        .unwrap();
        assert_eq!(
            shown(checks),
            [
                "queue-use kueue.x-k8s.io/localqueues/ use ns=team-b name=gpu",
                "workload-create ray.io/rayjobs/ create ns=team-b name=",
            ]
        );
    }

    #[test]
    fn the_tokens_namespace_is_the_default() {
        let run = principal(json!({"sub": "alice", "kube_user": "alice", "namespace": "team-a"}));
        let checks = tool_checks_for(
            &run,
            "workload_scale",
            json!({"kind": "Deployment", "name": "serve", "replicas": 2}),
        )
        .unwrap();
        assert_eq!(
            shown(checks),
            ["workload-scale apps/deployments/scale update ns=team-a name=serve"]
        );
    }

    #[test]
    fn tasks_are_checked_on_the_object_their_id_names() {
        let params = json!({"taskId": "team-c/Job/train/u1"});
        let get = KueueRules
            .checks(
                &Call::Method {
                    name: "tasks/get",
                    params: &params,
                },
                &person(),
            )
            .unwrap();
        assert_eq!(
            shown(get),
            ["task-get batch/jobs/ get ns=team-c name=train"]
        );
        let cancel = KueueRules
            .checks(
                &Call::Method {
                    name: "tasks/cancel",
                    params: &params,
                },
                &person(),
            )
            .unwrap();
        assert_eq!(
            shown(cancel),
            ["task-cancel batch/jobs/ update ns=team-c name=train"]
        );
    }

    #[test]
    fn a_caller_without_a_kubernetes_user_needs_no_checks() {
        let tenant = principal(json!({"sub": "tenant:r", "namespace": "team-a"}));
        assert!(
            tool_checks_for(
                &tenant,
                "workload_delete",
                json!({"kind": "Job", "name": "j"})
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn an_unresolvable_check_refuses_the_call() {
        for (tool, args) in [
            ("workload_status", json!({"kind": "Pod", "name": "x"})),
            ("workload_plan", json!({"queue": "q", "manifest": {}})),
            ("workload_delete", json!({"name": "x"})),
        ] {
            assert!(
                matches!(
                    tool_checks_for(&person(), tool, args),
                    Err(Refusal::ReviewError { .. })
                ),
                "{tool}"
            );
        }
        let bad = json!({"taskId": "nope"});
        assert!(matches!(
            KueueRules.checks(
                &Call::Method {
                    name: "tasks/get",
                    params: &bad
                },
                &person()
            ),
            Err(Refusal::ReviewError { .. })
        ));
    }
}

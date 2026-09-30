//! Starting points for a webhook form: a sender's verifier and a transform over its payload.

use crate::launches::webhooks::verify::VerifierKind;

pub struct Preset {
    pub id: &'static str,
    pub title: &'static str,
    pub verifier: VerifierKind,
    pub header: Option<&'static str>,
    pub filter: &'static str,
    pub dedupe: &'static str,
    /// Suggested derivations, keyed by a conventional param name the form maps onto the playbook's.
    pub derive: &'static [(&'static str, &'static str)],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "quay-push",
        title: "quay.io repository push",
        verifier: VerifierKind::PathToken,
        header: None,
        filter: "size(body.updated_tags) > 0",
        dedupe: "delivery",
        derive: &[
            ("repository", "body.repository"),
            ("image", "body.docker_url"),
            ("tags", "body.updated_tags"),
        ],
    },
    Preset {
        id: "github-release",
        title: "GitHub release published",
        verifier: VerifierKind::HmacSha256,
        header: Some("x-hub-signature-256"),
        filter: r#"headers["x-github-event"] == "release" && body.action == "published""#,
        dedupe: "string(body.release.id)",
        derive: &[
            ("repository", "body.repository.full_name"),
            ("tag", "body.release.tag_name"),
        ],
    },
    Preset {
        id: "github-push",
        title: "GitHub push",
        verifier: VerifierKind::HmacSha256,
        header: Some("x-hub-signature-256"),
        filter: r#"headers["x-github-event"] == "push" && !body.deleted"#,
        dedupe: "body.after",
        derive: &[
            ("repository", "body.repository.full_name"),
            ("ref", "body.ref"),
            ("sha", "body.after"),
        ],
    },
];

#[cfg(test)]
mod tests {
    use crate::launches::webhooks::presets::PRESETS;
    use crate::launches::webhooks::transform::{Input, Transform};
    use serde_json::json;

    fn run(id: &str, body: serde_json::Value, headers: serde_json::Value) -> (bool, String) {
        let preset = PRESETS.iter().find(|p| p.id == id).expect("preset");
        let derive = preset
            .derive
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect();
        let transform = Transform::compile(preset.filter, preset.dedupe, &derive)
            .unwrap_or_else(|e| panic!("{id} compiles: {e:?}"));
        let evaluated = transform.evaluate(&Input {
            delivery: "0199-a",
            body: &body,
            headers: headers.as_object().expect("object"),
            received_at: "2026-09-29T12:00:00Z",
        });
        assert!(evaluated.params.is_ok(), "{id}: {:?}", evaluated.params);
        (
            evaluated.filter.expect("filter"),
            evaluated.dedupe.expect("dedupe"),
        )
    }

    #[test]
    fn every_preset_compiles_and_runs_on_its_senders_payload() {
        let quay = json!({
            "name": "repository", "repository": "mynamespace/repository",
            "namespace": "mynamespace", "docker_url": "quay.io/mynamespace/repository",
            "homepage": "https://quay.io/repository/mynamespace/repository",
            "updated_tags": ["latest"]
        });
        assert_eq!(
            run("quay-push", quay, json!({})),
            (true, "0199-a".to_string())
        );

        let release = json!({
            "action": "published",
            "release": {"id": 1234, "tag_name": "v1.2.0"},
            "repository": {"full_name": "neuralmagic/crucible"}
        });
        assert_eq!(
            run(
                "github-release",
                release,
                json!({"x-github-event": "release"})
            ),
            (true, "1234".to_string())
        );

        let push = json!({
            "ref": "refs/heads/main", "after": "2520177", "deleted": false,
            "repository": {"full_name": "neuralmagic/crucible"}
        });
        assert_eq!(
            run("github-push", push, json!({"x-github-event": "push"})),
            (true, "2520177".to_string())
        );
    }
}

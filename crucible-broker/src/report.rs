//! Posts only the engine-authored report snapshot; tool callers supply no content.

const DEFAULT_TEMPLATE: &str = "{{ passed }} passed, {{ failed }} non-passing · ${{ '%.4f'|format(spent_usd) }}\n{%- for task in tasks %}\n• `{{ task.name }}` — {{ task.status }}{% endfor %}\n{%- if run_url %}\n<{{ run_url }}|Open run artifacts in Crucible>{% endif %}";
const DEFAULT_RESULT_MAX_BYTES: usize = 16 * 1024;
const MAX_RESULT_MAX_BYTES: usize = 64 * 1024;

#[derive(serde::Serialize)]
struct TemplateTask {
    name: String,
    status: String,
    cost_usd: f64,
}

#[derive(serde::Serialize)]
struct TemplateContext {
    run: String,
    run_url: Option<String>,
    tasks: Vec<TemplateTask>,
    passed: usize,
    failed: usize,
    spent_usd: f64,
}

fn slack_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn render(report: &crucible_contract::RunReport, template: Option<&str>) -> Result<String, String> {
    let passed = report.tasks.iter().filter(|t| t.status == "pass").count();
    let failed = report.tasks.iter().filter(|t| t.status != "pass").count();
    let spent: f64 = report.tasks.iter().map(|t| t.cost_usd).sum();
    let mut env = minijinja::Environment::new();
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    env.add_template("report", template.unwrap_or(DEFAULT_TEMPLATE))
        .map_err(|e| format!("compiling report template: {e}"))?;
    env.get_template("report")
        .and_then(|t| {
            t.render(TemplateContext {
                run: slack_escape(&report.run),
                run_url: report.run_url.as_deref().map(slack_escape),
                tasks: report
                    .tasks
                    .iter()
                    .take(20)
                    .map(|task| TemplateTask {
                        name: slack_escape(&task.name),
                        status: slack_escape(&task.status),
                        cost_usd: task.cost_usd,
                    })
                    .collect(),
                passed,
                failed,
                spent_usd: spent,
            })
        })
        .map_err(|e| format!("rendering report template: {e}"))
}

fn payload(
    report: &crucible_contract::RunReport,
    template: Option<&str>,
    result: Option<&str>,
) -> Result<serde_json::Value, String> {
    let failed = report.tasks.iter().filter(|t| t.status != "pass").count();
    let text = render(report, template)?;
    let mut blocks = vec![
        serde_json::json!({
            "type": "header",
            "text": {"type": "plain_text", "text": "Crucible workflow report"}
        }),
        serde_json::json!({
            "type": "section",
            "text": {"type": "mrkdwn", "text": format!(
                "*{}*\n{}",
                slack_escape(&report.run),
                text
            )}
        }),
    ];

    if let Some(name) = result {
        let selected = report
            .results
            .get(name)
            .ok_or_else(|| format!("selected report result {name:?} is absent"))?;
        blocks.push(serde_json::json!({"type": "divider"}));
        blocks.push(serde_json::json!({
            "type": "section",
            "fields": [
                {"type": "mrkdwn", "text": format!("*Result*\n{}", slack_escape(name))},
                {"type": "mrkdwn", "text": format!("*Status*\n{}", slack_escape(&selected.status))}
            ]
        }));
        if let Some(output) = &selected.output {
            let encoded = serde_json::to_vec(output)
                .map_err(|e| format!("encoding selected report result: {e}"))?;
            let limit = result_max_bytes()?;
            if encoded.len() > limit {
                return Err(format!(
                    "selected report result is {} bytes, exceeding the configured {limit}-byte limit",
                    encoded.len()
                ));
            }
            let object = output.as_object().ok_or_else(|| {
                "selected report result must be a JSON object for Slack cards".to_string()
            })?;
            for fields in object.iter().collect::<Vec<_>>().chunks(10) {
                let fields: Result<Vec<_>, String> = fields
                    .iter()
                    .map(|(key, value)| {
                        let value = card_value(value)?;
                        Ok(serde_json::json!({
                            "type": "mrkdwn",
                            "text": format!(
                                "*{}*\n{}",
                                slack_escape(&key.replace('_', " ")),
                                value
                            )
                        }))
                    })
                    .collect();
                blocks.push(serde_json::json!({"type": "section", "fields": fields?}));
            }
        }
    }

    if let Some(url) = &report.run_url {
        blocks.push(serde_json::json!({
            "type": "actions",
            "elements": [{
                "type": "button",
                "text": {"type": "plain_text", "text": "Open run in Crucible"},
                "url": url,
                "style": if failed == 0 { "primary" } else { "danger" }
            }]
        }));
    }
    Ok(serde_json::json!({"text": format!("Crucible workflow: {}", report.run), "blocks": blocks}))
}

fn result_max_bytes() -> Result<usize, String> {
    let Some(raw) = std::env::var("CRUCIBLE_REPORT_RESULT_MAX_BYTES").ok() else {
        return Ok(DEFAULT_RESULT_MAX_BYTES);
    };
    let value = raw
        .parse::<usize>()
        .map_err(|_| "CRUCIBLE_REPORT_RESULT_MAX_BYTES must be a positive integer".to_string())?;
    if value == 0 || value > MAX_RESULT_MAX_BYTES {
        return Err(format!(
            "CRUCIBLE_REPORT_RESULT_MAX_BYTES must be between 1 and {MAX_RESULT_MAX_BYTES}"
        ));
    }
    Ok(value)
}

fn card_value(value: &serde_json::Value) -> Result<String, String> {
    let raw = match value {
        serde_json::Value::String(value) => value.clone(),
        other => {
            serde_json::to_string(other).map_err(|e| format!("encoding Slack card field: {e}"))?
        }
    };
    let escaped = slack_escape(&raw);
    if escaped.len() > 2_000 {
        return Err("a selected report field exceeds Slack's 2000-character field limit".into());
    }
    Ok(escaped)
}

/// Deliver the engine-authored snapshot. Public so the workflow executor can enforce a
/// first-class `report()` task without routing it through an agent-controlled MCP call.
pub fn deliver(template: Option<&str>, result: Option<&str>) -> Result<String, String> {
    let path = forge::storage_root().join(crucible_contract::REPORT_FILE);
    let bytes = std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let report: crucible_contract::RunReport =
        serde_json::from_slice(&bytes).map_err(|e| format!("decoding engine report: {e}"))?;
    let body = payload(&report, template, result)?;
    let url =
        std::env::var("SLACK_WEBHOOK_URL").map_err(|_| "SLACK_WEBHOOK_URL is unset".to_string())?;
    crate::slack::post(&url, &body)?;
    Ok(r#"{"status":"delivered"}"#.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::sync::Mutex;

    static ENV: Mutex<()> = Mutex::new(());

    #[test]
    fn payload_contains_only_the_typed_engine_snapshot() {
        let report = crucible_contract::RunReport {
            run: "run-7".into(),
            run_url: Some("https://crucible.example/runs/run-7".into()),
            tasks: vec![crucible_contract::TaskReport {
                name: "roundup".into(),
                status: "pass".into(),
                cost_usd: 0.25,
            }],
            results: Default::default(),
        };
        let encoded = payload(&report, None, None).unwrap().to_string();
        assert!(encoded.contains("run-7"));
        assert!(encoded.contains("roundup"));
        assert!(encoded.contains("https://crucible.example/runs/run-7"));
        assert!(encoded.contains("Open run artifacts in Crucible"));
        assert!(!encoded.contains("prompt"));
        assert!(!encoded.contains("output"));
    }

    #[test]
    fn template_values_are_slack_escaped_before_rendering() {
        let report = crucible_contract::RunReport {
            run: "run<&".into(),
            run_url: Some("https://example.test/runs/7?a=1&b=2".into()),
            tasks: vec![crucible_contract::TaskReport {
                name: "task<@everyone>".into(),
                status: "pass".into(),
                cost_usd: 0.0,
            }],
            results: Default::default(),
        };
        let text = render(&report, Some("{{ run }} {{ tasks[0].name }} {{ run_url }}")).unwrap();
        assert_eq!(
            text,
            "run&lt;&amp; task&lt;@everyone&gt; https://example.test/runs/7?a=1&amp;b=2"
        );
    }

    #[test]
    fn selected_result_becomes_engine_owned_slack_blocks() {
        let report = crucible_contract::RunReport {
            run: "fips-watch".into(),
            run_url: Some("https://crucible.example/runs/fips-watch".into()),
            tasks: vec![crucible_contract::TaskReport {
                name: "card".into(),
                status: "pass".into(),
                cost_usd: 0.0,
            }],
            results: BTreeMap::from([(
                "card".into(),
                crucible_contract::ReportResult {
                    status: "pass".into(),
                    output: Some(serde_json::json!({
                        "verdict": "ACTION REQUIRED",
                        "dirty_variants": 3,
                        "crypto_blockers": ["ring"]
                    })),
                },
            )]),
        };

        let body = payload(&report, Some("*FIPS dependency watch*"), Some("card")).unwrap();
        let encoded = body.to_string();
        assert!(encoded.contains("\"blocks\""));
        assert!(encoded.contains("ACTION REQUIRED"));
        assert!(encoded.contains("dirty variants"));
        assert!(encoded.contains("Open run in Crucible"));
        assert!(!encoded.contains("webhook"));
    }

    #[test]
    fn deliver_posts_the_engine_snapshot_to_a_real_socket() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("crucible-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(crucible_contract::REPORT_FILE),
            serde_json::to_vec(&crucible_contract::RunReport {
                run: "run-9".into(),
                run_url: Some("https://crucible.example/runs/run-9".into()),
                tasks: vec![crucible_contract::TaskReport {
                    name: "roundup".into(),
                    status: "pass".into(),
                    cost_usd: 0.0,
                }],
                results: Default::default(),
            })
            .unwrap(),
        )
        .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0; 16 * 1024];
            let read = stream.read(&mut request).unwrap();
            request.truncate(read);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
            String::from_utf8_lossy(&request).to_string()
        });
        unsafe {
            std::env::set_var("FORGE_STORAGE_ROOT", &root);
            std::env::set_var("SLACK_WEBHOOK_URL", format!("http://{addr}/hook"));
        }
        assert!(deliver(None, None).unwrap().contains("delivered"));
        unsafe {
            std::env::remove_var("FORGE_STORAGE_ROOT");
            std::env::remove_var("SLACK_WEBHOOK_URL");
        }
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /hook"));
        assert!(request.contains("run-9"));
        assert!(request.contains("Open run artifacts in Crucible"));
        let _ = std::fs::remove_dir_all(root);
    }
}

//! Posts only the engine-authored report snapshot; tool callers supply no content.

const DEFAULT_TEMPLATE: &str = "{{ passed }} passed, {{ failed }} non-passing · ${{ '%.4f'|format(spent_usd) }}\n{%- for task in tasks %}\n• `{{ task.name }}` — {{ task.status }}{% endfor %}\n{%- if run_url %}\n<{{ run_url }}|Open run artifacts in Crucible>{% endif %}";

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
) -> Result<serde_json::Value, String> {
    let failed = report.tasks.iter().filter(|t| t.status != "pass").count();
    let text = render(report, template)?;
    Ok(serde_json::json!({"attachments": [{
        "color": if failed == 0 { "good" } else { "warning" },
        "title": format!("Crucible workflow: {}", slack_escape(&report.run)),
        "text": text,
        "mrkdwn_in": ["text"]
    }]}))
}

/// Deliver the engine-authored snapshot. Public so the workflow executor can enforce a
/// first-class `report()` task without routing it through an agent-controlled MCP call.
pub fn deliver(template: Option<&str>) -> Result<String, String> {
    let path = forge::storage_root().join(crucible_contract::REPORT_FILE);
    let bytes = std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let report: crucible_contract::RunReport =
        serde_json::from_slice(&bytes).map_err(|e| format!("decoding engine report: {e}"))?;
    let body = payload(&report, template)?;
    let url =
        std::env::var("SLACK_WEBHOOK_URL").map_err(|_| "SLACK_WEBHOOK_URL is unset".to_string())?;
    crate::slack::post(&url, &body)?;
    Ok(r#"{"status":"delivered"}"#.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        };
        let encoded = payload(&report, None).unwrap().to_string();
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
        };
        let text = render(&report, Some("{{ run }} {{ tasks[0].name }} {{ run_url }}")).unwrap();
        assert_eq!(
            text,
            "run&lt;&amp; task&lt;@everyone&gt; https://example.test/runs/7?a=1&amp;b=2"
        );
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
        assert!(deliver(None).unwrap().contains("delivered"));
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

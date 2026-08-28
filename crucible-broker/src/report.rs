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
    render_with(report, template, slack_escape)
}

fn render_plain(
    report: &crucible_contract::RunReport,
    template: Option<&str>,
) -> Result<String, String> {
    render_with(report, template, str::to_owned)
}

fn render_with(
    report: &crucible_contract::RunReport,
    template: Option<&str>,
    escape: fn(&str) -> String,
) -> Result<String, String> {
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
                run: escape(&report.run),
                run_url: report.run_url.as_deref().map(escape),
                tasks: report
                    .tasks
                    .iter()
                    .take(20)
                    .map(|task| TemplateTask {
                        name: escape(&task.name),
                        status: escape(&task.status),
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

fn selected_string(
    report: &crucible_contract::RunReport,
    task: &str,
    field: &str,
) -> Result<String, String> {
    report
        .results
        .get(task)
        .and_then(|result| result.output.as_ref())
        .and_then(|output| output.get(field))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("selected report output {task}.{field} is absent or not a string"))
}

fn render_slack_variables(
    report: &crucible_contract::RunReport,
    template: Option<&str>,
    variables: &std::collections::BTreeMap<String, String>,
) -> Result<String, String> {
    let passed = report
        .tasks
        .iter()
        .filter(|task| task.status == "pass")
        .count();
    let failed = report.tasks.len() - passed;
    let spent_usd = report.tasks.iter().map(|task| task.cost_usd).sum::<f64>();
    let mut context = serde_json::json!({
        "run": slack_escape(&report.run),
        "run_url": report.run_url.as_deref().map(slack_escape),
        "tasks": report.tasks.iter().take(20).map(|task| serde_json::json!({
            "name": slack_escape(&task.name),
            "status": slack_escape(&task.status),
            "cost_usd": task.cost_usd,
        })).collect::<Vec<_>>(),
        "passed": passed,
        "failed": failed,
        "spent_usd": spent_usd,
    });
    let object = context
        .as_object_mut()
        .expect("report context is an object");
    for (name, value) in variables {
        if object.contains_key(name) {
            return Err(format!("report variable {name:?} shadows an engine field"));
        }
        object.insert(name.clone(), serde_json::Value::String(slack_escape(value)));
    }
    let mut env = minijinja::Environment::new();
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    env.add_template("report", template.unwrap_or(DEFAULT_TEMPLATE))
        .map_err(|error| format!("compiling report template: {error}"))?;
    env.get_template("report")
        .and_then(|template| template.render(context))
        .map_err(|error| format!("rendering report template: {error}"))
}

fn markdown_payload(
    report: &crucible_contract::RunReport,
    text: String,
) -> Result<serde_json::Value, String> {
    if text.trim().is_empty() {
        return Err("report markdown is empty".into());
    }
    let characters = text.chars().count();
    if characters > 12_000 {
        return Err(format!(
            "report markdown is {characters} characters; Slack markdown blocks allow at most 12000"
        ));
    }
    Ok(serde_json::json!({
        "text": format!("Crucible workflow {}: {}", slack_escape(&report.run), text),
        "blocks": [{"type": "markdown", "text": text}],
    }))
}

/// Deliver the engine-authored snapshot. Public so the workflow executor can enforce a
/// first-class `report()` task without routing it through an agent-controlled MCP call.
pub fn deliver(template: Option<&str>, result: Option<&str>) -> Result<String, String> {
    let report = read_report()?;
    let body = payload(&report, template, result)?;
    let url =
        std::env::var("SLACK_WEBHOOK_URL").map_err(|_| "SLACK_WEBHOOK_URL is unset".to_string())?;
    crate::slack::post(&url, &body)?;
    Ok(r#"{"status":"delivered"}"#.to_string())
}

pub fn deliver_slack(
    template: Option<&str>,
    result: Option<&str>,
    markdown_from: Option<(&str, &str)>,
    variable_refs: &std::collections::BTreeMap<String, (String, String)>,
) -> Result<String, String> {
    if markdown_from.is_none() && variable_refs.is_empty() {
        return deliver(template, result);
    }
    let report = read_report()?;
    let text = if let Some((task, field)) = markdown_from {
        slack_escape(&selected_string(&report, task, field)?)
    } else {
        let variables = variable_refs
            .iter()
            .map(|(name, (task, field))| {
                selected_string(&report, task, field).map(|value| (name.clone(), value))
            })
            .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
        render_slack_variables(&report, template, &variables)?
    };
    let body = markdown_payload(&report, text)?;
    let url =
        std::env::var("SLACK_WEBHOOK_URL").map_err(|_| "SLACK_WEBHOOK_URL is unset".to_string())?;
    crate::slack::post(&url, &body)?;
    Ok(r#"{"status":"delivered"}"#.to_string())
}

fn read_report() -> Result<crucible_contract::RunReport, String> {
    let path = forge::storage_root().join(crucible_contract::REPORT_FILE);
    let bytes = std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("decoding engine report: {e}"))
}

fn selected_result<'a>(
    report: &'a crucible_contract::RunReport,
    result: Option<&str>,
) -> Result<(String, String), String> {
    let Some(name) = result else {
        return Ok((String::new(), String::new()));
    };
    let selected = report
        .results
        .get(name)
        .ok_or_else(|| format!("selected report result {name:?} is absent"))?;
    let json = match &selected.output {
        Some(output) => {
            let encoded = serde_json::to_vec(output)
                .map_err(|e| format!("encoding selected report result: {e}"))?;
            let limit = result_max_bytes()?;
            if encoded.len() > limit {
                return Err(format!(
                    "selected report result is {} bytes, exceeding the configured {limit}-byte limit",
                    encoded.len()
                ));
            }
            String::from_utf8(encoded)
                .map_err(|e| format!("encoding selected report result: {e}"))?
        }
        None => String::new(),
    };
    Ok((selected.status.clone(), json))
}

fn docs_request(
    report: &crucible_contract::RunReport,
    template: Option<&str>,
    result: Option<&str>,
) -> Result<serde_json::Value, String> {
    let mut text = format!(
        "\n\nCrucible workflow report: {}\n{}",
        report.run,
        render_plain(report, template)?
    );
    if let Some(name) = result {
        let (status, json) = selected_result(report, Some(name))?;
        text.push_str(&format!("\nResult {name}: {status}"));
        if !json.is_empty() {
            text.push('\n');
            text.push_str(&json);
        }
    }
    Ok(serde_json::json!({
        "requests": [{
            "insertText": {
                "endOfSegmentLocation": {},
                "text": text
            }
        }]
    }))
}

fn sheets_request(path: &std::path::Path) -> Result<serde_json::Value, String> {
    let extension = path.extension().and_then(|value| value.to_str());
    let delimiter = match extension {
        Some("csv") => b',',
        Some("tsv") => b'\t',
        _ => return Err("Google Sheets report input must end in .csv or .tsv".to_string()),
    };
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .delimiter(delimiter)
        .from_path(path)
        .map_err(|e| format!("opening Google Sheets report input {}: {e}", path.display()))?;
    let mut values = Vec::new();
    let mut width = None;
    for record in reader.records() {
        let record = record.map_err(|e| format!("parsing Google Sheets report input: {e}"))?;
        match width {
            None => width = Some(record.len()),
            Some(expected) if expected != record.len() => {
                return Err(format!(
                    "Google Sheets report input has {} fields in a row; expected {expected}",
                    record.len()
                ));
            }
            Some(_) => {}
        }
        values.push(record.iter().map(str::to_owned).collect::<Vec<_>>());
    }
    if values.is_empty() {
        return Err("Google Sheets report input is empty".to_string());
    }
    Ok(serde_json::json!({"values": values}))
}

async fn google_token() -> Result<String, String> {
    const SCOPES: &[&str] = &[
        "https://www.googleapis.com/auth/documents",
        "https://www.googleapis.com/auth/spreadsheets",
        "https://www.googleapis.com/auth/drive.file",
    ];
    let provider = gcp_auth::provider()
        .await
        .map_err(|e| format!("resolving Google application default credentials: {e}"))?;
    provider
        .token(SCOPES)
        .await
        .map(|token| token.as_str().to_owned())
        .map_err(|e| format!("minting Google Workspace access token: {e}"))
}

fn google_post(url: &str, body: &serde_json::Value) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("creating Google Workspace runtime: {e}"))?;
    runtime.block_on(async {
        let token = google_token().await?;
        let client = google_workspace::client::shared_client()
            .map_err(|e| format!("building gws HTTP client: {e}"))?;
        let response = google_workspace::client::send_with_retry(|| {
            client.post(url).bearer_auth(&token).json(body)
        })
        .await
        .map_err(|e| format!("posting Google Workspace report: {e}"))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "Google Workspace report returned HTTP {}",
                response.status()
            ))
        }
    })
}

fn google_service(service: &str) -> Result<(String, String), String> {
    google_workspace::services::resolve_service(service)
        .map_err(|e| format!("resolving gws service {service:?}: {e}"))
}

pub fn deliver_google_docs(template: Option<&str>, result: Option<&str>) -> Result<String, String> {
    let report = read_report()?;
    let document = std::env::var("CRUCIBLE_GOOGLE_DOC_ID")
        .map_err(|_| "CRUCIBLE_GOOGLE_DOC_ID is unset".to_string())?;
    let (service, version) = google_service("docs")?;
    let document = google_workspace::validate::encode_path_segment(&document);
    let url =
        format!("https://{service}.googleapis.com/{version}/documents/{document}:batchUpdate");
    google_post(&url, &docs_request(&report, template, result)?)?;
    Ok(r#"{"status":"delivered"}"#.to_string())
}

pub fn deliver_google_sheets(path: &std::path::Path) -> Result<String, String> {
    let spreadsheet = std::env::var("CRUCIBLE_GOOGLE_SHEET_ID")
        .map_err(|_| "CRUCIBLE_GOOGLE_SHEET_ID is unset".to_string())?;
    let range = std::env::var("CRUCIBLE_GOOGLE_SHEET_RANGE")
        .map_err(|_| "CRUCIBLE_GOOGLE_SHEET_RANGE is unset".to_string())?;
    let (service, version) = google_service("sheets")?;
    let spreadsheet = google_workspace::validate::encode_path_segment(&spreadsheet);
    let range = google_workspace::validate::encode_path_segment(&range);
    let url = format!(
        "https://{service}.googleapis.com/{version}/spreadsheets/{spreadsheet}/values/{range}:append?valueInputOption=RAW&insertDataOption=INSERT_ROWS"
    );
    google_post(&url, &sheets_request(path)?)?;
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
    fn selected_markdown_is_escaped_and_sent_as_a_markdown_block() {
        let report = crucible_contract::RunReport {
            run: "fips-watch".into(),
            run_url: None,
            tasks: vec![],
            results: BTreeMap::from([(
                "card".into(),
                crucible_contract::ReportResult {
                    status: "pass".into(),
                    output: Some(serde_json::json!({
                        "markdown": "## FIPS CLEAN\n- ping <!channel> and <@U123>"
                    })),
                },
            )]),
        };
        let text = slack_escape(&selected_string(&report, "card", "markdown").unwrap());
        let body = markdown_payload(&report, text).unwrap();
        assert_eq!(body["blocks"][0]["type"], "markdown");
        assert_eq!(
            body["blocks"][0]["text"],
            "## FIPS CLEAN\n- ping &lt;!channel&gt; and &lt;@U123&gt;"
        );
    }

    fn google_report() -> crucible_contract::RunReport {
        crucible_contract::RunReport {
            run: "run<&".into(),
            run_url: Some("https://crucible.example/runs/7".into()),
            tasks: vec![crucible_contract::TaskReport {
                name: "roundup".into(),
                status: "pass".into(),
                cost_usd: 0.5,
            }],
            results: BTreeMap::from([(
                "roundup".into(),
                crucible_contract::ReportResult {
                    status: "pass".into(),
                    output: Some(serde_json::json!({"finding": "=IMPORTDATA(\"bad\")"})),
                },
            )]),
        }
    }

    #[test]
    fn docs_projection_is_plain_insert_text() {
        let body = docs_request(&google_report(), Some("{{ run }}"), Some("roundup")).unwrap();
        let text = body["requests"][0]["insertText"]["text"].as_str().unwrap();
        assert!(text.contains("run<&"));
        assert!(text.contains("=IMPORTDATA"));
        assert_eq!(
            body["requests"][0]["insertText"]["endOfSegmentLocation"],
            serde_json::json!({})
        );
        assert!(body.get("documentId").is_none());
    }

    #[test]
    fn sheets_projection_parses_csv_as_a_rectangular_raw_matrix() {
        let root = std::env::temp_dir().join(format!("crucible-sheet-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("report.csv");
        std::fs::write(&path, "name,value\nformula,\"=IMPORTDATA(\"\"bad\"\")\"\n").unwrap();
        let body = sheets_request(&path).unwrap();
        assert_eq!(body["values"][0], serde_json::json!(["name", "value"]));
        assert_eq!(
            body["values"][1],
            serde_json::json!(["formula", "=IMPORTDATA(\"bad\")"])
        );

        let (service, version) = google_service("sheets").unwrap();
        let range = google_workspace::validate::encode_path_segment("Reports!A:H");
        let url = format!(
            "https://{service}.googleapis.com/{version}/spreadsheets/sheet-id/values/{range}:append?valueInputOption=RAW"
        );
        assert!(url.contains("Reports%21A%3AH:append"));
        assert!(url.contains("valueInputOption=RAW"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sheets_projection_rejects_ragged_tsv_before_delivery() {
        let path = std::env::temp_dir().join(format!("crucible-sheet-{}.tsv", std::process::id()));
        std::fs::write(&path, "a\tb\nc\n").unwrap();
        let error = sheets_request(&path).unwrap_err();
        assert!(error.contains("found record with 1 fields"), "{error}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    #[ignore = "writes to operator-configured Google Workspace test resources"]
    fn live_google_workspace_delivery_with_adc() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        for name in [
            "GOOGLE_APPLICATION_CREDENTIALS",
            "CRUCIBLE_GOOGLE_DOC_ID",
            "CRUCIBLE_GOOGLE_SHEET_ID",
            "CRUCIBLE_GOOGLE_SHEET_RANGE",
        ] {
            assert!(std::env::var_os(name).is_some(), "{name} must be set");
        }

        let root =
            std::env::temp_dir().join(format!("crucible-google-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(crucible_contract::REPORT_FILE),
            serde_json::to_vec(&google_report()).unwrap(),
        )
        .unwrap();
        let sheet_input = root.join("report.csv");
        std::fs::write(
            &sheet_input,
            "source,status,formula\nlive-service-account-test,pass,=1+1\n",
        )
        .unwrap();
        unsafe { std::env::set_var("FORGE_STORAGE_ROOT", &root) };

        assert_eq!(
            deliver_google_docs(
                Some("Live service-account smoke: {{ run }}"),
                Some("roundup")
            )
            .unwrap(),
            r#"{"status":"delivered"}"#
        );
        assert_eq!(
            deliver_google_sheets(&sheet_input).unwrap(),
            r#"{"status":"delivered"}"#
        );

        unsafe { std::env::remove_var("FORGE_STORAGE_ROOT") };
        let _ = std::fs::remove_dir_all(root);
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

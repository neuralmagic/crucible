//! Posts only the engine-authored report snapshot; tool callers supply no content.

use std::path::PathBuf;

const DEFAULT_TEMPLATE: &str = "{% for task in tasks %}• `{{ task.name }}` — {{ task.status }}\n{% endfor %}\n{%- if run_url %}<{{ run_url }}|Open run artifacts in Crucible>{% endif %}";
const DEFAULT_RESULT_MAX_BYTES: usize = 16 * 1024;
const MAX_RESULT_MAX_BYTES: usize = 64 * 1024;
const MAX_BODY_MAX_BYTES: usize = 3_000;
const MAX_TEMPLATE_TASKS: usize = 20;
const RESULT_MAX_ENV: &str = "CRUCIBLE_REPORT_RESULT_MAX_BYTES";
const BODY_MAX_ENV: &str = "CRUCIBLE_REPORT_BODY_MAX_BYTES";

#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("reading {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("decoding engine report: {0}")]
    Decode(serde_json::Error),
    #[error("{name} must be an integer between 1 and {max}, got {got:?}")]
    InvalidLimit {
        name: &'static str,
        max: usize,
        got: String,
    },
    #[error("selected report result {name:?} is absent")]
    AbsentResult { name: String },
    #[error("encoding selected report result: {0}")]
    Encode(serde_json::Error),
    #[error(
        "selected report result is {size} bytes, exceeding the configured {limit}-byte limit (CRUCIBLE_REPORT_RESULT_MAX_BYTES)"
    )]
    OversizedResult { size: usize, limit: usize },
    #[error("selected report result must be a JSON object for Slack cards")]
    NonObjectResult,
    #[error("selected report field {field:?} exceeds Slack's 2000-character field limit")]
    OversizedField { field: String },
    #[error("compiling report template: {0}")]
    Compile(minijinja::Error),
    #[error("rendering report template: {0}")]
    Render(minijinja::Error),
    #[error(
        "rendered report body is {size} bytes, exceeding the configured {limit}-byte limit (CRUCIBLE_REPORT_BODY_MAX_BYTES)"
    )]
    OversizedBody { size: usize, limit: usize },
    #[error("SLACK_WEBHOOK_URL is unset")]
    MissingWebhook,
    #[error("{0}")]
    Delivery(String),
}

/// The report's selected task and, optionally, the declared field whose value picks the accent.
#[derive(Clone, Copy, Debug)]
pub struct Selection<'a> {
    pub task: &'a str,
    pub severity_field: Option<&'a str>,
}

#[derive(Clone, Copy, Debug)]
struct Limits {
    result_max_bytes: usize,
    body_max_bytes: usize,
}

impl Limits {
    fn from_env() -> Result<Self, ReportError> {
        Ok(Self {
            result_max_bytes: limit_from_env(
                RESULT_MAX_ENV,
                DEFAULT_RESULT_MAX_BYTES,
                MAX_RESULT_MAX_BYTES,
            )?,
            body_max_bytes: limit_from_env(BODY_MAX_ENV, MAX_BODY_MAX_BYTES, MAX_BODY_MAX_BYTES)?,
        })
    }
}

fn limit_from_env(name: &'static str, default: usize, max: usize) -> Result<usize, ReportError> {
    let Ok(raw) = std::env::var(name) else {
        return Ok(default);
    };
    match raw.parse::<usize>() {
        Ok(value) if (1..=max).contains(&value) => Ok(value),
        _ => Err(ReportError::InvalidLimit {
            name,
            max,
            got: raw,
        }),
    }
}

struct Totals {
    passed: usize,
    failed: usize,
    spent_usd: f64,
}

impl Totals {
    fn of(report: &crucible_contract::RunReport) -> Self {
        let passed = report.tasks.iter().filter(|t| t.status == "pass").count();
        Self {
            passed,
            failed: report.tasks.len() - passed,
            spent_usd: report.tasks.iter().map(|t| t.cost_usd).sum(),
        }
    }
}

/// The card's left-edge colour. Only these values reach the payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Accent {
    Neutral,
    Good,
    Warning,
    Danger,
}

impl Accent {
    fn color(self) -> Option<&'static str> {
        match self {
            Accent::Neutral => None,
            Accent::Good => Some("good"),
            Accent::Warning => Some("warning"),
            Accent::Danger => Some("danger"),
        }
    }

    fn of(selected: &crucible_contract::ReportResult, field: &str) -> Self {
        match selected.status.as_str() {
            "pass" => match selected
                .output
                .as_ref()
                .and_then(|output| output.get(field))
                .and_then(serde_json::Value::as_str)
            {
                Some("good") => Accent::Good,
                Some("warning") => Accent::Warning,
                Some("danger") => Accent::Danger,
                _ => Accent::Neutral,
            },
            "fail" | "transport" | "blocked" => Accent::Danger,
            _ => Accent::Neutral,
        }
    }
}

struct Selected<'a> {
    name: &'a str,
    result: &'a crucible_contract::ReportResult,
}

fn select<'a>(
    report: &'a crucible_contract::RunReport,
    name: &'a str,
    limits: Limits,
) -> Result<Selected<'a>, ReportError> {
    let result = report
        .results
        .get(name)
        .ok_or_else(|| ReportError::AbsentResult {
            name: name.to_owned(),
        })?;
    if let Some(output) = &result.output {
        let size = serde_json::to_vec(output)
            .map_err(ReportError::Encode)?
            .len();
        if size > limits.result_max_bytes {
            return Err(ReportError::OversizedResult {
                size,
                limit: limits.result_max_bytes,
            });
        }
    }
    Ok(Selected { name, result })
}

#[derive(serde::Serialize)]
struct TemplateTask<'a> {
    name: &'a str,
    status: &'a str,
    cost_usd: f64,
}

#[derive(serde::Serialize)]
struct TemplateResult<'a> {
    name: &'a str,
    status: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<minijinja::Value>,
}

#[derive(serde::Serialize)]
struct TemplateContext<'a> {
    run: &'a str,
    run_url: Option<&'a str>,
    verdict: crucible_contract::RunVerdict,
    tasks: Vec<TemplateTask<'a>>,
    passed: usize,
    failed: usize,
    spent_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<TemplateResult<'a>>,
}

fn slack_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn template_value(value: &serde_json::Value) -> minijinja::Value {
    match value {
        serde_json::Value::Null => minijinja::Value::from(()),
        serde_json::Value::Bool(value) => minijinja::Value::from(*value),
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                minijinja::Value::from(value)
            } else if let Some(value) = number.as_u64() {
                minijinja::Value::from(value)
            } else if let Some(value) = number.as_f64() {
                minijinja::Value::from(value)
            } else {
                minijinja::Value::from(number.to_string())
            }
        }
        serde_json::Value::String(value) => minijinja::Value::from(value.as_str()),
        serde_json::Value::Array(items) => {
            minijinja::Value::from(items.iter().map(template_value).collect::<Vec<_>>())
        }
        serde_json::Value::Object(object) => object
            .iter()
            .map(|(key, value)| (key.as_str(), template_value(value)))
            .collect(),
    }
}

fn render(
    report: &crucible_contract::RunReport,
    totals: &Totals,
    template: &str,
    selected: Option<&Selected<'_>>,
    limits: Limits,
) -> Result<String, ReportError> {
    let mut env = minijinja::Environment::new();
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    // Every inserted value is escaped here, after filters run, so neither `|safe` nor a
    // string-building filter can emit Slack control sequences.
    env.set_formatter(|out, _state, value| {
        out.write_str(&slack_escape(&value.to_string()))
            .map_err(minijinja::Error::from)
    });
    env.add_template("report", template)
        .map_err(ReportError::Compile)?;
    let context = TemplateContext {
        run: &report.run,
        run_url: report.run_url.as_deref(),
        verdict: report.verdict,
        tasks: report
            .tasks
            .iter()
            .take(MAX_TEMPLATE_TASKS)
            .map(|task| TemplateTask {
                name: &task.name,
                status: &task.status,
                cost_usd: task.cost_usd,
            })
            .collect(),
        passed: totals.passed,
        failed: totals.failed,
        spent_usd: totals.spent_usd,
        result: selected.map(|selected| TemplateResult {
            name: selected.name,
            status: &selected.result.status,
            output: selected
                .result
                .output
                .as_ref()
                .filter(|_| selected.result.status == "pass")
                .map(template_value),
        }),
    };
    let body = env
        .get_template("report")
        .and_then(|t| t.render(context))
        .map_err(ReportError::Render)?;
    if body.len() > limits.body_max_bytes {
        return Err(ReportError::OversizedBody {
            size: body.len(),
            limit: limits.body_max_bytes,
        });
    }
    Ok(body)
}

fn payload(
    report: &crucible_contract::RunReport,
    template: Option<&str>,
    selection: Option<Selection<'_>>,
    limits: Limits,
) -> Result<serde_json::Value, ReportError> {
    let totals = Totals::of(report);
    let selected = selection
        .map(|selection| select(report, selection.task, limits))
        .transpose()?;
    let body = render(
        report,
        &totals,
        template.unwrap_or(DEFAULT_TEMPLATE),
        selected.as_ref(),
        limits,
    )?;
    let mut blocks = vec![
        serde_json::json!({
            "type": "header",
            "text": {"type": "plain_text", "text": "Crucible workflow report"}
        }),
        serde_json::json!({
            "type": "section",
            "text": {"type": "mrkdwn", "text": format!("*{}*", slack_escape(&report.run))}
        }),
        serde_json::json!({
            "type": "context",
            "elements": [{"type": "mrkdwn", "text": format!(
                "*Verdict* {} · {} passed, {} non-passing · ${:.4}",
                report.verdict.as_str(),
                totals.passed,
                totals.failed,
                totals.spent_usd
            )}]
        }),
    ];
    if !body.trim().is_empty() {
        blocks.push(serde_json::json!({
            "type": "section",
            "text": {"type": "mrkdwn", "text": body}
        }));
    }

    if let Some(selected) = &selected {
        blocks.push(serde_json::json!({"type": "divider"}));
        blocks.push(serde_json::json!({
            "type": "section",
            "fields": [
                {"type": "mrkdwn", "text": format!("*Result*\n{}", slack_escape(selected.name))},
                {"type": "mrkdwn", "text": format!("*Status*\n{}", slack_escape(&selected.result.status))}
            ]
        }));
        if let Some(output) = &selected.result.output {
            let object = output.as_object().ok_or(ReportError::NonObjectResult)?;
            for fields in object.iter().collect::<Vec<_>>().chunks(10) {
                let fields: Result<Vec<_>, ReportError> = fields
                    .iter()
                    .map(|(key, value)| {
                        let value = card_value(key, value)?;
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
        let mut button = serde_json::json!({
            "type": "button",
            "text": {"type": "plain_text", "text": "Open run in Crucible"},
            "url": url
        });
        let style = match report.verdict {
            crucible_contract::RunVerdict::Pass => Some("primary"),
            crucible_contract::RunVerdict::Fail => Some("danger"),
            crucible_contract::RunVerdict::Pending => None,
        };
        if let (Some(style), Some(object)) = (style, button.as_object_mut()) {
            object.insert("style".to_owned(), serde_json::Value::from(style));
        }
        blocks.push(serde_json::json!({"type": "actions", "elements": [button]}));
    }
    let text = format!("Crucible workflow: {}", slack_escape(&report.run));
    let accent = selection
        .zip(selected.as_ref())
        .and_then(|(selection, selected)| {
            selection
                .severity_field
                .map(|field| Accent::of(selected.result, field))
        });
    Ok(match accent {
        None => serde_json::json!({"text": text, "blocks": blocks}),
        Some(accent) => {
            let mut attachment = serde_json::json!({"blocks": blocks});
            if let (Some(color), Some(object)) = (accent.color(), attachment.as_object_mut()) {
                object.insert("color".to_owned(), serde_json::Value::from(color));
            }
            serde_json::json!({"text": text, "attachments": [attachment]})
        }
    })
}

fn card_value(field: &str, value: &serde_json::Value) -> Result<String, ReportError> {
    let raw = match value {
        serde_json::Value::String(value) => value.clone(),
        other => serde_json::to_string(other).map_err(ReportError::Encode)?,
    };
    let escaped = slack_escape(&raw);
    if escaped.len() > 2_000 {
        return Err(ReportError::OversizedField {
            field: field.to_owned(),
        });
    }
    Ok(escaped)
}

/// Deliver the engine-authored snapshot. Public so the workflow executor can enforce a
/// first-class `report()` task without routing it through an agent-controlled MCP call.
pub fn deliver(
    template: Option<&str>,
    selection: Option<Selection<'_>>,
) -> Result<String, ReportError> {
    let path = forge::storage_root().join(crucible_contract::REPORT_FILE);
    let bytes = std::fs::read(&path).map_err(|source| ReportError::Read {
        path: path.clone(),
        source,
    })?;
    let report: crucible_contract::RunReport =
        serde_json::from_slice(&bytes).map_err(ReportError::Decode)?;
    let body = payload(&report, template, selection, Limits::from_env()?)?;
    let url = std::env::var("SLACK_WEBHOOK_URL").map_err(|_| ReportError::MissingWebhook)?;
    crate::slack::post(&url, &body).map_err(ReportError::Delivery)?;
    Ok(r#"{"status":"delivered"}"#.to_string())
}

#[cfg(test)]
mod tests {
    use crate::report::*;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};

    const LIMITS: Limits = Limits {
        result_max_bytes: DEFAULT_RESULT_MAX_BYTES,
        body_max_bytes: MAX_BODY_MAX_BYTES,
    };

    fn task(name: &str, status: &str) -> crucible_contract::TaskReport {
        crucible_contract::TaskReport {
            name: name.into(),
            status: status.into(),
            cost_usd: 0.0,
            blocked: None,
            transport: None,
        }
    }

    fn selected_report(
        status: &str,
        output: Option<serde_json::Value>,
    ) -> crucible_contract::RunReport {
        crucible_contract::RunReport {
            run: "watch".into(),
            run_url: None,
            tasks: vec![task("card", status)],
            verdict: crucible_contract::RunVerdict::Pass,
            results: BTreeMap::from([(
                "card".into(),
                crucible_contract::ReportResult {
                    status: status.into(),
                    output,
                },
            )]),
        }
    }

    fn render_selected(
        report: &crucible_contract::RunReport,
        template: &str,
    ) -> Result<String, ReportError> {
        let selected = select(report, "card", LIMITS)?;
        render(
            report,
            &Totals::of(report),
            template,
            Some(&selected),
            LIMITS,
        )
    }

    fn severity(status: &str, value: serde_json::Value) -> serde_json::Value {
        let report = selected_report(
            status,
            (status == "pass").then(|| serde_json::json!({"severity": value})),
        );
        payload(
            &report,
            Some("body"),
            Some(Selection {
                task: "card",
                severity_field: Some("severity"),
            }),
            LIMITS,
        )
        .unwrap()
    }

    #[test]
    fn payload_contains_only_the_typed_engine_snapshot() {
        let report = crucible_contract::RunReport {
            run: "run-7".into(),
            run_url: Some("https://crucible.example/runs/run-7".into()),
            tasks: vec![crucible_contract::TaskReport {
                name: "roundup".into(),
                status: "pass".into(),
                cost_usd: 0.25,
                blocked: None,
                transport: None,
            }],
            verdict: crucible_contract::RunVerdict::Pass,
            results: Default::default(),
        };
        let encoded = payload(&report, None, None, LIMITS).unwrap().to_string();
        assert!(encoded.contains("run-7"));
        assert!(encoded.contains("roundup"));
        assert!(encoded.contains("https://crucible.example/runs/run-7"));
        assert!(encoded.contains("Open run artifacts in Crucible"));
        assert!(encoded.contains("*Verdict* pass · 1 passed, 0 non-passing · $0.2500"));
        assert!(!encoded.contains("prompt"));
        assert!(!encoded.contains("output"));
        assert!(!encoded.contains("attachments"));
    }

    #[test]
    fn template_values_are_slack_escaped_before_rendering() {
        let report = crucible_contract::RunReport {
            run: "run<&".into(),
            run_url: Some("https://example.test/runs/7?a=1&b=2".into()),
            tasks: vec![task("task<@everyone>", "pass")],
            verdict: crucible_contract::RunVerdict::Pending,
            results: Default::default(),
        };
        let text = render(
            &report,
            &Totals::of(&report),
            "{{ run }} {{ tasks[0].name }} {{ run_url }}",
            None,
            LIMITS,
        )
        .unwrap();
        assert_eq!(
            text,
            "run&lt;&amp; task&lt;@everyone&gt; https://example.test/runs/7?a=1&amp;b=2"
        );
    }

    #[test]
    fn a_hostile_result_value_renders_as_literal_text_whatever_the_template_does() {
        let hostile = "<!channel> <https://x|y> & co";
        let report = selected_report("pass", Some(serde_json::json!({"note": hostile})));
        let text = render_selected(
            &report,
            "{{ result.output.note }}|{{ result.output.note|safe }}|{{ result.output.note|upper }}|\
             {{ result.output.note ~ '' }}|{{ result.output }}|{% for k, v in result.output|items %}{{ v }}{% endfor %}",
        )
        .unwrap();
        assert!(!text.contains('<'), "{text}");
        assert!(!text.contains('>'), "{text}");
        assert!(
            text.starts_with("&lt;!channel&gt; &lt;https://x|y&gt; &amp; co|&lt;!channel&gt;"),
            "{text}"
        );

        let body = payload(
            &report,
            Some("{{ result.output.note }}"),
            Some(Selection {
                task: "card",
                severity_field: None,
            }),
            LIMITS,
        )
        .unwrap()
        .to_string();
        assert!(!body.contains("<!channel>"), "{body}");
        assert!(!body.contains("<https://x|y>"), "{body}");
        assert!(
            body.contains("&lt;!channel&gt; &lt;https://x|y&gt; &amp; co"),
            "{body}"
        );
    }

    #[test]
    fn a_template_reads_declared_fields_with_their_json_types_when_the_selected_task_passed() {
        let report = selected_report(
            "pass",
            Some(serde_json::json!({
                "verdict": "ACTION REQUIRED",
                "dirty_variants": 3,
                "ratio": 0.5,
                "clean": false,
                "blockers": ["ring", "aws-lc"]
            })),
        );
        let text = render_selected(
            &report,
            "{{ result.name }} {{ result.status }} {{ result.output.verdict }} \
             {{ result.output.dirty_variants + 1 }} {{ result.output.ratio * 2 }} \
             {{ result.output.clean is false }} {{ result.output.blockers|length }}",
        )
        .unwrap();
        assert_eq!(text, "card pass ACTION REQUIRED 4 1.0 True 2");
    }

    #[test]
    fn a_template_gets_only_the_terminal_status_of_a_selected_task_that_did_not_pass() {
        let report = selected_report("fail", Some(serde_json::json!({"verdict": "partial"})));
        let text = render_selected(
            &report,
            "{{ result.status }} {{ result.output is defined }}",
        )
        .unwrap();
        assert_eq!(text, "fail False");
        let error = render_selected(&report, "{{ result.output.verdict }}").unwrap_err();
        assert!(matches!(error, ReportError::Render(_)), "{error}");
    }

    #[test]
    fn a_template_gets_no_value_for_a_field_absent_from_the_snapshot() {
        let report = selected_report("pass", Some(serde_json::json!({"verdict": "ok"})));
        let text = render_selected(&report, "{{ result.output.secret is defined }}").unwrap();
        assert_eq!(text, "False");
        let error = render_selected(&report, "{{ result.output.secret }}").unwrap_err();
        assert!(matches!(error, ReportError::Render(_)), "{error}");
    }

    #[test]
    fn a_template_reads_the_verdict_cost_and_bounded_task_context() {
        let mut report = selected_report("pass", None);
        report.tasks = (0..25)
            .map(|index| crucible_contract::TaskReport {
                cost_usd: 0.5,
                ..task(
                    &format!("t{index}"),
                    if index == 3 { "fail" } else { "pass" },
                )
            })
            .collect();
        report.verdict = crucible_contract::RunVerdict::Fail;
        let text = render(
            &report,
            &Totals::of(&report),
            "{{ verdict }} {{ '%.2f'|format(spent_usd) }} {{ passed }}/{{ failed }} {{ tasks|length }} {{ tasks[3].status }}",
            None,
            LIMITS,
        )
        .unwrap();
        assert_eq!(text, "fail 12.50 24/1 20 fail");
    }

    #[test]
    fn the_verdict_is_the_engines_not_a_count_of_non_passing_tasks() {
        let mut report = selected_report("pass", None);
        report.run_url = Some("https://crucible.example/runs/watch".into());
        report.tasks.extend([
            task("other_route", "not_taken"),
            task("optional", "skipped"),
            task("advisory_lint", "fail"),
        ]);
        let body = payload(&report, Some("{{ verdict }}"), None, LIMITS).unwrap();
        assert_eq!(body["blocks"][3]["text"]["text"], "pass");
        let encoded = body.to_string();
        assert!(
            encoded.contains("*Verdict* pass · 1 passed, 3 non-passing"),
            "{encoded}"
        );
        assert!(encoded.contains("\"style\":\"primary\""), "{encoded}");
    }

    #[test]
    fn a_report_written_before_the_main_graph_settled_reads_pending() {
        let mut report: crucible_contract::RunReport = serde_json::from_value(serde_json::json!({
            "run": "watch",
            "run_url": "https://crucible.example/runs/watch",
            "tasks": [{"name": "card", "status": "fail", "cost_usd": 0.0}]
        }))
        .unwrap();
        assert_eq!(report.verdict, crucible_contract::RunVerdict::Pending);
        let body = payload(&report, Some("{{ verdict }}"), None, LIMITS).unwrap();
        assert_eq!(body["blocks"][3]["text"]["text"], "pending");
        let encoded = body.to_string();
        assert!(encoded.contains("*Verdict* pending"), "{encoded}");
        assert!(!encoded.contains("\"style\""), "{encoded}");
        report.verdict = crucible_contract::RunVerdict::Fail;
        let encoded = payload(&report, None, None, LIMITS).unwrap().to_string();
        assert!(encoded.contains("\"style\":\"danger\""), "{encoded}");
    }

    #[test]
    fn a_rendered_body_over_the_configured_size_fails_without_truncation() {
        let report = selected_report("pass", Some(serde_json::json!({"note": "x".repeat(40)})));
        let limits = Limits {
            result_max_bytes: DEFAULT_RESULT_MAX_BYTES,
            body_max_bytes: 32,
        };
        let error = payload(
            &report,
            Some("{{ result.output.note }}"),
            Some(Selection {
                task: "card",
                severity_field: None,
            }),
            limits,
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                ReportError::OversizedBody {
                    size: 40,
                    limit: 32
                }
            ),
            "{error}"
        );
        let message = error.to_string();
        assert!(message.contains("32-byte limit"), "{message}");
        assert!(message.contains(BODY_MAX_ENV), "{message}");
    }

    #[test]
    fn body_and_result_limits_are_read_from_the_operator_environment() {
        let _guard = crate::test_support::env_lock();
        unsafe {
            std::env::set_var(BODY_MAX_ENV, "128");
            std::env::remove_var(RESULT_MAX_ENV);
        }
        let limits = Limits::from_env().unwrap();
        assert_eq!(limits.body_max_bytes, 128);
        assert_eq!(limits.result_max_bytes, DEFAULT_RESULT_MAX_BYTES);
        for bad in ["0", "3001", "lots"] {
            unsafe { std::env::set_var(BODY_MAX_ENV, bad) };
            let error = Limits::from_env().unwrap_err();
            assert!(
                matches!(
                    error,
                    ReportError::InvalidLimit {
                        name: BODY_MAX_ENV,
                        max: 3_000,
                        ..
                    }
                ),
                "{bad}: {error}"
            );
        }
        unsafe { std::env::remove_var(BODY_MAX_ENV) };
        assert_eq!(
            Limits::from_env().unwrap().body_max_bytes,
            MAX_BODY_MAX_BYTES
        );
    }

    #[test]
    fn severity_field_sets_the_accent_the_selected_result_names() {
        for (value, color) in [
            ("good", "good"),
            ("warning", "warning"),
            ("danger", "danger"),
        ] {
            let body = severity("pass", serde_json::json!(value));
            assert_eq!(body["attachments"][0]["color"], color, "{value}");
            assert!(body.get("blocks").is_none());
            assert_eq!(body["attachments"][0]["blocks"][0]["type"], "header");
        }
    }

    #[test]
    fn severity_field_selects_the_accent_whatever_the_run_verdict() {
        let mut report = selected_report("pass", Some(serde_json::json!({"severity": "good"})));
        report.tasks.push(task("lint", "fail"));
        report.verdict = crucible_contract::RunVerdict::Fail;
        report.run_url = Some("https://crucible.example/runs/watch".into());
        let body = payload(
            &report,
            Some("body"),
            Some(Selection {
                task: "card",
                severity_field: Some("severity"),
            }),
            LIMITS,
        )
        .unwrap();
        assert_eq!(body["attachments"][0]["color"], "good");
        let encoded = body.to_string();
        assert!(encoded.contains("*Verdict* fail"), "{encoded}");
        assert!(encoded.contains("\"style\":\"danger\""), "{encoded}");
    }

    #[test]
    fn any_other_severity_value_or_an_unrun_selected_task_renders_neutral() {
        for value in [
            serde_json::json!("GOOD"),
            serde_json::json!("critical"),
            serde_json::json!(1),
            serde_json::json!(null),
            serde_json::json!(["danger"]),
        ] {
            let body = severity("pass", value.clone());
            assert!(body["attachments"][0].get("color").is_none(), "{value}");
        }
        for status in ["skipped", "not_taken"] {
            let body = severity(status, serde_json::json!("danger"));
            assert!(body["attachments"][0].get("color").is_none(), "{status}");
        }
        let missing = payload(
            &selected_report("pass", Some(serde_json::json!({}))),
            Some("body"),
            Some(Selection {
                task: "card",
                severity_field: Some("severity"),
            }),
            LIMITS,
        )
        .unwrap();
        assert!(missing["attachments"][0].get("color").is_none());
    }

    #[test]
    fn a_selected_task_that_failed_transport_failed_or_blocked_renders_danger() {
        for status in ["fail", "transport", "blocked"] {
            let body = severity(status, serde_json::json!("good"));
            assert_eq!(body["attachments"][0]["color"], "danger", "{status}");
        }
    }

    #[test]
    fn a_hostile_severity_value_renders_neutral_and_reaches_the_card_only_as_literal_text() {
        let body = severity("pass", serde_json::json!("danger<!channel>"));
        assert!(body["attachments"][0].get("color").is_none());
        let encoded = body.to_string();
        assert!(!encoded.contains("<!channel>"), "{encoded}");
        assert_eq!(
            encoded.matches("danger&lt;!channel&gt;").count(),
            1,
            "{encoded}"
        );
        assert_eq!(
            body["attachments"][0]["blocks"][6]["fields"][0]["text"],
            "*severity*\ndanger&lt;!channel&gt;"
        );
    }

    #[test]
    fn selected_result_becomes_engine_owned_slack_blocks() {
        let report = crucible_contract::RunReport {
            run: "fips-watch".into(),
            run_url: Some("https://crucible.example/runs/fips-watch".into()),
            tasks: vec![task("card", "pass")],
            verdict: crucible_contract::RunVerdict::Pass,
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

        let body = payload(
            &report,
            Some("*FIPS dependency watch*"),
            Some(Selection {
                task: "card",
                severity_field: None,
            }),
            LIMITS,
        )
        .unwrap();
        let encoded = body.to_string();
        assert!(encoded.contains("\"blocks\""));
        assert!(encoded.contains("ACTION REQUIRED"));
        assert!(encoded.contains("dirty variants"));
        assert!(encoded.contains("Open run in Crucible"));
        assert!(!encoded.contains("webhook"));
    }

    #[test]
    fn an_oversized_selected_result_fails_before_rendering() {
        let report = selected_report("pass", Some(serde_json::json!({"note": "x".repeat(64)})));
        let limits = Limits {
            result_max_bytes: 16,
            body_max_bytes: MAX_BODY_MAX_BYTES,
        };
        let error = payload(
            &report,
            Some("{{ undefined_name }}"),
            Some(Selection {
                task: "card",
                severity_field: None,
            }),
            limits,
        )
        .unwrap_err();
        assert!(
            matches!(error, ReportError::OversizedResult { limit: 16, .. }),
            "{error}"
        );
    }

    #[test]
    fn deliver_posts_the_engine_snapshot_to_a_real_socket() {
        let _guard = crate::test_support::env_lock();
        let root = std::env::temp_dir().join(format!("crucible-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(crucible_contract::REPORT_FILE),
            serde_json::to_vec(&crucible_contract::RunReport {
                run: "run-9".into(),
                run_url: Some("https://crucible.example/runs/run-9".into()),
                tasks: vec![task("roundup", "pass")],
                verdict: crucible_contract::RunVerdict::Pass,
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

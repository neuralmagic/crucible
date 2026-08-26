use std::time::Duration;

pub(crate) fn post(url: &str, body: &serde_json::Value) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("building Slack client: {e}"))?;
    let response = client
        .post(url)
        .json(body)
        .send()
        .map_err(|e| format!("posting Slack report: {e}"))?;
    response
        .status()
        .is_success()
        .then_some(())
        .ok_or_else(|| format!("Slack rejected report with {}", response.status()))
}

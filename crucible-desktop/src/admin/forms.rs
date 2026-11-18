//! The management pages' forms, and the checks on what is typed into them.

use crux::dto;
use rune::TextEdit;

/// Which cursor a test firing is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorFrom {
    Stored,
    None,
    Value,
}

impl CursorFrom {
    pub const ALL: [CursorFrom; 3] = [CursorFrom::Stored, CursorFrom::None, CursorFrom::Value];

    pub fn label(self) -> &'static str {
        match self {
            CursorFrom::Stored => "Stored",
            CursorFrom::None => "None",
            CursorFrom::Value => "Value",
        }
    }
}

/// The test-fire form open on one schedule: which cursor its firing gets.
pub struct TestFireForm {
    pub schedule: String,
    pub from: CursorFrom,
    pub value: TextEdit,
}

impl TestFireForm {
    pub fn on(schedule: &str) -> Self {
        TestFireForm {
            schedule: schedule.to_string(),
            from: CursorFrom::Stored,
            value: TextEdit::new(""),
        }
    }

    /// The cursor to send, or `None` while a value is asked for and none is typed.
    pub fn ask(&self) -> Option<dto::TestCursor> {
        match self.from {
            CursorFrom::Stored => Some(dto::TestCursor::Stored),
            CursorFrom::None => Some(dto::TestCursor::None),
            CursorFrom::Value => (!self.value.text.is_empty()).then(|| dto::TestCursor::Value {
                value: self.value.text.clone(),
            }),
        }
    }
}

/// The launch sheet's schedule half: when it fires, in which zone, owned by whom, and the
/// schedule it edits.
pub struct ScheduleForm {
    pub stored: Option<Box<dto::Schedule>>,
    pub cron: TextEdit,
    pub tz: TextEdit,
    pub owner: String,
}

impl ScheduleForm {
    pub fn new(owner: &str) -> ScheduleForm {
        let tz = jiff::tz::TimeZone::system()
            .iana_name()
            .unwrap_or("UTC")
            .to_string();
        ScheduleForm {
            stored: None,
            cron: TextEdit::new("0 9 * * MON-FRI"),
            tz: TextEdit::new(tz),
            owner: owner.to_string(),
        }
    }

    pub fn editing(schedule: &dto::Schedule) -> ScheduleForm {
        ScheduleForm {
            cron: TextEdit::new(&schedule.cron_expr),
            tz: TextEdit::new(if schedule.tz.is_empty() {
                "UTC"
            } else {
                &schedule.tz
            }),
            owner: schedule.owner_principal.clone().unwrap_or_default(),
            stored: Some(Box::new(schedule.clone())),
        }
    }

    /// The body that creates the schedule, or replaces the stored one keeping what the form
    /// does not show.
    pub fn body(
        &self,
        target: &crate::live::Target,
        params: &serde_json::Value,
        max_cost: f64,
        max_time: &str,
        digest: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let cron = self.cron.text.split_whitespace().collect::<Vec<_>>();
        if cron.len() != 5 {
            return Err("A cron expression has five fields, like 0 9 * * MON-FRI".to_string());
        }
        let tz = self.tz.text.trim();
        if tz.is_empty() {
            return Err("A time zone, like UTC or America/New_York".to_string());
        }
        let params = params
            .as_object()
            .map(dto::string_params)
            .unwrap_or_default();
        let mut body = match &self.stored {
            Some(stored) => stored.body(stored.enabled),
            None => serde_json::json!({"advance_dedupe": true, "enabled": true}),
        };
        let target_kind = match target {
            crate::live::Target::Playbook(_) => "adopted",
            crate::live::Target::Draft(_) => "draft_head",
        };
        let fields = [
            ("owner", serde_json::json!(self.owner)),
            ("playbook", serde_json::json!(target.id())),
            ("target_kind", serde_json::json!(target_kind)),
            ("params", serde_json::Value::Object(params)),
            ("max_cost", serde_json::json!(max_cost)),
            ("max_time", serde_json::json!(max_time)),
            ("cron_expr", serde_json::json!(cron.join(" "))),
            ("tz", serde_json::json!(tz)),
        ];
        for (name, value) in fields {
            body[name] = value;
        }
        if let Some(digest) = digest {
            body["schema_digest"] = serde_json::json!(digest);
        }
        Ok(body)
    }
}

/// The secret kinds a value can be typed in for, with what the form calls them.
pub const SECRET_KINDS: [(&str, &str); 6] = [
    ("opaque", "Token"),
    ("github_token", "GitHub token"),
    ("inference_api_key", "Model API key"),
    ("file", "File"),
    ("registry_authfile", "Registry auth"),
    ("kubeconfig", "Kubeconfig"),
];

/// A secret being registered from a typed value.
pub struct SecretForm {
    pub name: TextEdit,
    pub owner: String,
    pub kind: usize,
    pub agent_visible: bool,
    pub value: TextEdit,
    /// The environment variable a model API key lands in.
    pub variable: TextEdit,
}

impl SecretForm {
    pub fn new(owner: &str) -> SecretForm {
        SecretForm {
            name: TextEdit::new(""),
            owner: owner.to_string(),
            kind: 0,
            agent_visible: false,
            value: TextEdit::new(""),
            variable: TextEdit::new("OPENAI_API_KEY"),
        }
    }

    pub fn kind(&self) -> &'static str {
        SECRET_KINDS[self.kind.min(SECRET_KINDS.len() - 1)].0
    }

    /// Whether the value is a document rather than one line.
    pub fn multiline(&self) -> bool {
        matches!(self.kind(), "file" | "registry_authfile" | "kubeconfig")
    }

    /// The registration body, or what is wrong with the form.
    pub fn body(&self) -> Result<serde_json::Value, String> {
        let name = self.name.text.trim();
        if name.is_empty() || name.len() > 96 {
            return Err("A name of 1 to 96 characters".to_string());
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        {
            return Err("Names are lowercase letters, digits, '.', '_' and '-'".to_string());
        }
        if self.value.text.is_empty() {
            return Err("No value".to_string());
        }
        let value = match self.kind() {
            "inference_api_key" => {
                let variable = self.variable.text.trim();
                let valid = variable
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
                    && variable
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                if !valid {
                    return Err(format!("{variable:?} is not an environment variable name"));
                }
                serde_json::json!({ variable: self.value.text.trim() }).to_string()
            }
            "opaque" | "github_token" => self.value.text.trim().to_string(),
            _ => self.value.text.clone(),
        };
        let visible = self.agent_visible && self.kind() == "opaque";
        Ok(serde_json::json!({
            "name": name,
            "owner": self.owner,
            "kind": self.kind(),
            "visibility": if visible { "agent_visible" } else { "broker_only" },
            "value": value,
        }))
    }
}

/// What a share form's `until` sends: the end of that day in UTC, blank for no end, or why it is
/// not a date.
pub fn not_after(until: &str) -> Result<Option<String>, String> {
    let day = until.trim();
    if day.is_empty() {
        return Ok(None);
    }
    day.parse::<jiff::civil::Date>()
        .map(|date| Some(format!("{date}T23:59:59Z")))
        .map_err(|_| format!("{day} is not a date like 2026-12-31"))
}

/// Whether `grantee` names a principal a share can go to.
pub fn valid_grantee(grantee: &str) -> bool {
    match grantee.trim().split_once(':') {
        Some(("user" | "team", name)) => {
            !name.is_empty() && !name.contains(':') && !name.chars().any(char::is_whitespace)
        }
        _ => false,
    }
}

//! The provider form: which service a registration talks to, the models it serves, the agent CLI
//! it runs under and the secret it authenticates with.

use crux::dto::{InferenceProtocol, Provider, ProviderBody, ProviderKind, RegisterProvider};
use rune::TextEdit;

pub const KINDS: [(ProviderKind, &str); 4] = [
    (ProviderKind::Openai, "openai"),
    (ProviderKind::Anthropic, "anthropic"),
    (ProviderKind::Vertex, "vertex"),
    (ProviderKind::Custom, "custom"),
];

pub const PROTOCOLS: [(InferenceProtocol, &str); 5] = [
    (InferenceProtocol::ChatCompletions, "chat_completions"),
    (InferenceProtocol::Responses, "responses"),
    (InferenceProtocol::Messages, "messages"),
    (InferenceProtocol::Decisions, "decisions"),
    (InferenceProtocol::SystemOne, "system_one"),
];

pub fn kind_of(wire: &str) -> ProviderKind {
    KINDS
        .iter()
        .find(|(_, label)| *label == wire)
        .map_or(ProviderKind::Custom, |(kind, _)| *kind)
}

pub fn kind_label(kind: ProviderKind) -> &'static str {
    KINDS
        .iter()
        .find(|(held, _)| *held == kind)
        .map_or("custom", |(_, label)| label)
}

pub fn protocol_label(protocol: InferenceProtocol) -> &'static str {
    PROTOCOLS
        .iter()
        .find(|(held, _)| *held == protocol)
        .map_or("chat_completions", |(_, label)| label)
}

/// The agent CLIs a registration may run under, the default first. A decision protocol runs
/// none. Mirrors the controller's own table, so the form offers only what the API accepts.
pub fn harnesses(kind: ProviderKind, protocol: InferenceProtocol) -> &'static [&'static str] {
    match (kind, protocol) {
        (ProviderKind::Anthropic, _) => &["claude", "opencode", "pi"],
        (ProviderKind::Vertex, _) => &["claude"],
        (ProviderKind::Openai, _) => &["codex", "opencode", "pi"],
        (ProviderKind::Custom, InferenceProtocol::Messages) => &["claude", "opencode", "pi"],
        (ProviderKind::Custom, InferenceProtocol::ChatCompletions) => &["opencode", "pi"],
        (ProviderKind::Custom, InferenceProtocol::Responses) => &["codex", "pi"],
        (ProviderKind::Custom, InferenceProtocol::Decisions | InferenceProtocol::SystemOne) => &[],
    }
}

/// What one line of the list says a provider speaks and where.
pub fn reach(provider: &Provider) -> String {
    if provider.kind != "custom" {
        return provider.kind.clone();
    }
    format!(
        "{} at {}",
        provider.protocol.map_or("?", protocol_label),
        provider.endpoint.as_deref().unwrap_or("?")
    )
}

pub fn may(provider: &Provider, action: &str) -> bool {
    provider.actions.iter().any(|held| held == action)
}

pub struct ProviderForm {
    /// The registration being edited, by id; a new one otherwise.
    pub editing: Option<String>,
    pub id: TextEdit,
    pub owner: String,
    pub name: TextEdit,
    pub kind: ProviderKind,
    /// One model per line or comma; blank takes the kind's own list.
    pub models: TextEdit,
    pub default_model: TextEdit,
    pub endpoint: TextEdit,
    pub protocol: InferenceProtocol,
    /// The agent CLI to run under; `None` takes the default.
    pub harness: Option<&'static str>,
    pub secret: TextEdit,
    pub secret_owner: TextEdit,
    pub enabled: bool,
}

impl ProviderForm {
    pub fn new(owner: &str) -> ProviderForm {
        ProviderForm {
            editing: None,
            id: TextEdit::new(""),
            owner: owner.to_string(),
            name: TextEdit::new(""),
            kind: ProviderKind::Openai,
            models: TextEdit::new(""),
            default_model: TextEdit::new(""),
            endpoint: TextEdit::new(""),
            protocol: InferenceProtocol::ChatCompletions,
            harness: None,
            secret: TextEdit::new(""),
            secret_owner: TextEdit::new(""),
            enabled: true,
        }
    }

    pub fn editing(provider: &Provider) -> ProviderForm {
        let kind = kind_of(&provider.kind);
        let protocol = provider.protocol.unwrap_or_default();
        let harness = provider.harness_override.as_deref().and_then(|named| {
            harnesses(kind, protocol)
                .iter()
                .copied()
                .find(|held| *held == named)
        });
        ProviderForm {
            editing: Some(provider.id.clone()),
            id: TextEdit::new(&provider.id),
            owner: provider.owner.clone(),
            name: TextEdit::new(provider.display_name.as_deref().unwrap_or_default()),
            kind,
            models: TextEdit::new(provider.models.join("\n")),
            default_model: TextEdit::new(&provider.default_model),
            endpoint: TextEdit::new(provider.endpoint.as_deref().unwrap_or_default()),
            protocol,
            harness,
            secret: TextEdit::new(provider.secret_name.as_deref().unwrap_or_default()),
            secret_owner: TextEdit::new(provider.secret_owner.as_deref().unwrap_or_default()),
            enabled: provider.enabled,
        }
    }

    /// Set the kind; a harness the new service cannot run goes back to the default.
    pub fn set_kind(&mut self, kind: ProviderKind) {
        self.kind = kind;
        self.keep_harness();
    }

    pub fn set_protocol(&mut self, protocol: InferenceProtocol) {
        self.protocol = protocol;
        self.keep_harness();
    }

    fn keep_harness(&mut self) {
        let allowed = harnesses(self.kind, self.protocol);
        self.harness = self.harness.filter(|held| allowed.contains(held));
    }

    pub fn model_list(&self) -> Vec<String> {
        self.models
            .text
            .split(['\n', ','])
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// The registration as a save sends it, or the first thing wrong with the form.
    pub fn body(&self) -> Result<ProviderBody, String> {
        let name = self.name.text.trim();
        if name.is_empty() {
            return Err("Name it".into());
        }
        let models = self.model_list();
        if let Some(bad) = models.iter().find(|model| !model_name(model)) {
            return Err(format!("\"{bad}\" is not a model name"));
        }
        let default_model = self.default_model.text.trim();
        if !default_model.is_empty() && !model_name(default_model) {
            return Err(format!("\"{default_model}\" is not a model name"));
        }
        let custom = self.kind == ProviderKind::Custom;
        let endpoint = self.endpoint.text.trim();
        if custom {
            if default_model.is_empty() {
                return Err("A custom provider names its default model".into());
            }
            let absolute = ["http://", "https://"]
                .iter()
                .any(|scheme| endpoint.len() > scheme.len() && endpoint.starts_with(scheme));
            if !absolute || endpoint.contains(char::is_whitespace) {
                return Err("The endpoint is an absolute http(s) URL".into());
            }
        }
        let secret = self.secret.text.trim();
        let secret_owner = self.secret_owner.text.trim();
        if self.kind == ProviderKind::Vertex && !secret.is_empty() {
            return Err("A vertex provider takes no secret".into());
        }
        if !secret_owner.is_empty() && secret.is_empty() {
            return Err("A secret owner needs a secret name".into());
        }
        let text = |value: &str| (!value.is_empty()).then(|| value.to_string());
        Ok(ProviderBody {
            display_name: name.to_string(),
            kind: self.kind,
            models,
            default_model: text(default_model),
            endpoint: text(endpoint).filter(|_| custom),
            protocol: custom.then_some(self.protocol),
            harness: self.harness.map(str::to_string),
            secret_name: text(secret),
            secret_owner: text(secret_owner).filter(|_| !secret.is_empty()),
            enabled: self.enabled,
        })
    }

    /// A new registration: its id and owner with [`ProviderForm::body`].
    pub fn register(&self) -> Result<RegisterProvider, String> {
        let id = self.id.text.trim();
        let plain = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
        if id.is_empty() || id.len() > 64 || !id.chars().all(plain) {
            return Err("The id is 1 to 64 lowercase letters, digits and dashes".into());
        }
        Ok(RegisterProvider {
            id: id.to_string(),
            owner: self.owner.clone(),
            provider: self.body()?,
        })
    }
}

/// A letter or digit, then letters, digits and `._:/-`.
fn model_name(model: &str) -> bool {
    let mut chars = model.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || "._:/-".contains(c))
}

#[cfg(test)]
mod tests {
    use crux::dto::{InferenceProtocol, Provider, ProviderKind};
    use rune::TextEdit;

    use crate::provider::{ProviderForm, harnesses, kind_of, may, reach};

    fn stored() -> Provider {
        serde_json::from_str(
            r#"{"id":"lab","display_name":"Lab vLLM","kind":"custom","models":["glm-5-3","glm-5-2"],
                "default_model":"glm-5-3","harness":"pi","harness_override":"pi",
                "endpoint":"https://vllm.example/v1","protocol":"chat_completions",
                "secret_name":"lab-key","secret_owner":"team:infra","enabled":false,
                "owner":"team:infra","actions":["read","update"]}"#,
        )
        .unwrap()
    }

    fn hosted(name: &str) -> ProviderForm {
        let mut form = ProviderForm::new("user:wynn");
        form.id = TextEdit::new("openai");
        form.name = TextEdit::new(name);
        form
    }

    #[test]
    fn a_hosted_kind_sends_no_endpoint_protocol_or_blank_fields() {
        let sent = hosted(" OpenAI ").register().unwrap();
        assert_eq!(
            (sent.id.as_str(), sent.owner.as_str()),
            ("openai", "user:wynn")
        );
        let body = sent.provider;
        assert_eq!(body.display_name, "OpenAI");
        assert_eq!(body.kind, ProviderKind::Openai);
        assert!(body.models.is_empty() && body.enabled);
        assert_eq!(
            (
                body.default_model,
                body.endpoint,
                body.protocol,
                body.harness,
                body.secret_name
            ),
            (None, None, None, None, None)
        );
    }

    #[test]
    fn editing_reads_a_stored_registration_back_into_the_same_body() {
        let provider = stored();
        assert_eq!(
            reach(&provider),
            "chat_completions at https://vllm.example/v1"
        );
        assert!(may(&provider, "update") && !may(&provider, "delete"));
        let form = ProviderForm::editing(&provider);
        assert_eq!(form.editing.as_deref(), Some("lab"));
        assert_eq!(form.harness, Some("pi"));
        let body = form.body().unwrap();
        assert_eq!(body.kind, ProviderKind::Custom);
        assert_eq!(body.models, ["glm-5-3", "glm-5-2"]);
        assert_eq!(body.default_model.as_deref(), Some("glm-5-3"));
        assert_eq!(body.endpoint.as_deref(), Some("https://vllm.example/v1"));
        assert_eq!(body.protocol, Some(InferenceProtocol::ChatCompletions));
        assert_eq!(body.harness.as_deref(), Some("pi"));
        assert_eq!(
            (body.secret_name.as_deref(), body.secret_owner.as_deref()),
            (Some("lab-key"), Some("team:infra"))
        );
        assert!(!body.enabled);
    }

    #[test]
    fn a_harness_the_new_service_cannot_run_goes_back_to_the_default() {
        let mut form = ProviderForm::editing(&stored());
        form.set_protocol(InferenceProtocol::Responses);
        assert_eq!(form.harness, Some("pi"));
        form.set_protocol(InferenceProtocol::Decisions);
        assert_eq!(form.harness, None);
        form.harness = Some("codex");
        form.set_kind(ProviderKind::Vertex);
        assert_eq!(form.harness, None);
        assert_eq!(
            harnesses(ProviderKind::Vertex, InferenceProtocol::Messages),
            ["claude"]
        );
        assert!(harnesses(ProviderKind::Custom, InferenceProtocol::SystemOne).is_empty());
        assert_eq!(kind_of("anthropic"), ProviderKind::Anthropic);
        assert_eq!(kind_of("something-new"), ProviderKind::Custom);
    }

    #[test]
    fn the_form_refuses_what_the_controller_would() {
        let refused = |change: fn(&mut ProviderForm)| {
            let mut form = hosted("OpenAI");
            change(&mut form);
            form.register().unwrap_err()
        };
        assert!(refused(|f| f.id = TextEdit::new("Open AI")).contains("id"));
        assert!(refused(|f| f.id = TextEdit::new("a".repeat(65))).contains("id"));
        assert!(refused(|f| f.name = TextEdit::new("  ")).contains("Name"));
        assert!(refused(|f| f.models = TextEdit::new("gpt-5, bad model")).contains("bad model"));
        assert!(refused(|f| f.default_model = TextEdit::new("-x")).contains("-x"));
        assert!(refused(|f| f.kind = ProviderKind::Custom).contains("default model"));
        assert!(
            refused(|f| {
                f.kind = ProviderKind::Custom;
                f.default_model = TextEdit::new("glm-5-3");
                f.endpoint = TextEdit::new("vllm.example/v1");
            })
            .contains("endpoint")
        );
        assert!(
            refused(|f| {
                f.kind = ProviderKind::Vertex;
                f.secret = TextEdit::new("key");
            })
            .contains("vertex")
        );
        assert!(refused(|f| f.secret_owner = TextEdit::new("team:infra")).contains("secret name"));
    }

    #[test]
    fn models_split_on_lines_and_commas() {
        let mut form = hosted("OpenAI");
        form.models = TextEdit::new("gpt-5.1,\n  o4-mini \n\nmeta/llama:70b");
        assert_eq!(form.model_list(), ["gpt-5.1", "o4-mini", "meta/llama:70b"]);
        assert_eq!(form.body().unwrap().models.len(), 3);
    }
}

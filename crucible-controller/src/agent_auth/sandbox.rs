//! A sandbox's gateway-assigned id, as a binding, the directory and the token endpoint name it.

const MAX_SANDBOX_ID_LEN: usize = 128;

/// An OpenShell sandbox id: the gateway-assigned id, also the last segment of the sandbox
/// supervisor's SPIFFE ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxId(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("sandbox id {0:?} is not 1-{MAX_SANDBOX_ID_LEN} characters of [A-Za-z0-9._-]")]
pub struct SandboxIdError(pub String);

impl SandboxId {
    pub fn parse(raw: &str) -> Result<Self, SandboxIdError> {
        let valid = !raw.is_empty()
            && raw.len() <= MAX_SANDBOX_ID_LEN
            && raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        match valid {
            true => Ok(Self(raw.to_string())),
            false => Err(SandboxIdError(raw.to_string())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SandboxId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

//! The environment a pack-authored command runs with outside the sandbox (RFC-0001:C-COMMANDS):
//! a fixed base plus exactly the variables its manifest table declares. Nothing else of the
//! engine's environment reaches it, so a script the agent can edit cannot read the engine's own
//! credentials.

use std::ffi::OsString;
use std::fmt;
use std::process::Command;

/// What every such command receives from the engine's environment, when the engine has it.
pub const BASE: [&str; 4] = ["PATH", "HOME", "LANG", "TMPDIR"];

/// A variable a manifest table declares its commands receive.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize, serde::Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct EnvName(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "{name:?} is not an environment variable name; a name is a letter or underscore followed by \
     letters, digits, and underscores"
)]
pub struct EnvNameError {
    name: String,
}

impl TryFrom<String> for EnvName {
    type Error = EnvNameError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        let mut chars = name.chars();
        let valid = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if valid {
            Ok(EnvName(name))
        } else {
            Err(EnvNameError { name })
        }
    }
}

impl From<EnvName> for String {
    fn from(name: EnvName) -> Self {
        name.0
    }
}

impl EnvName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EnvName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Declared variables the engine's environment does not set. A command that depends on one
/// would run without it, so the run refuses instead.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "{table}.env declares {}, which the engine's environment does not set",
    .missing.join(", ")
)]
pub struct MissingEnv {
    table: &'static str,
    missing: Vec<String>,
}

/// The declared variables one manifest table's commands receive, with the values the engine had
/// when the run resolved them.
#[derive(Clone, Default)]
pub struct HostEnv {
    declared: Vec<(EnvName, OsString)>,
}

impl fmt::Debug for HostEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.declared.iter().map(|(name, _)| name))
            .finish()
    }
}

impl HostEnv {
    /// Resolve `table`'s declared names against the engine's environment.
    pub fn resolve(table: &'static str, declared: &[EnvName]) -> Result<HostEnv, MissingEnv> {
        Self::resolve_with(table, declared, |name| std::env::var_os(name))
    }

    fn resolve_with(
        table: &'static str,
        declared: &[EnvName],
        lookup: impl Fn(&str) -> Option<OsString>,
    ) -> Result<HostEnv, MissingEnv> {
        let mut found = Vec::with_capacity(declared.len());
        let mut missing = Vec::new();
        for name in declared {
            match lookup(name.as_str()) {
                Some(value) => found.push((name.clone(), value)),
                None => missing.push(name.0.clone()),
            }
        }
        if missing.is_empty() {
            Ok(HostEnv { declared: found })
        } else {
            Err(MissingEnv { table, missing })
        }
    }

    /// Replace `cmd`'s environment with the base and the declared variables. Variables the
    /// engine sets for the command itself go on afterwards.
    pub fn apply(&self, cmd: &mut Command) {
        cmd.env_clear();
        for name in BASE {
            if let Some(value) = std::env::var_os(name) {
                cmd.env(name, value);
            }
        }
        for (name, value) in &self.declared {
            cmd.env(name.as_str(), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::host_env::{EnvName, HostEnv, MissingEnv};
    use std::ffi::OsString;

    fn names(names: &[&str]) -> Vec<EnvName> {
        names
            .iter()
            .map(|n| EnvName::try_from((*n).to_owned()).unwrap())
            .collect()
    }

    #[test]
    fn a_name_is_a_letter_or_underscore_then_letters_digits_and_underscores() {
        for good in ["KUBECONFIG", "_X", "a1_B2"] {
            assert!(EnvName::try_from(good.to_owned()).is_ok(), "{good}");
        }
        for bad in ["", "1X", "A-B", "A=B", "A B", "Ä"] {
            let error = EnvName::try_from(bad.to_owned()).unwrap_err();
            assert!(error.to_string().contains(&format!("{bad:?}")), "{error}");
        }
    }

    #[test]
    fn every_declared_variable_the_engine_lacks_is_named_at_once() {
        let lookup = |name: &str| (name == "KUBECONFIG").then(|| OsString::from("/k"));
        let error =
            HostEnv::resolve_with("[judge]", &names(&["GH_TOKEN", "KUBECONFIG", "X"]), lookup)
                .unwrap_err();
        assert_eq!(
            error,
            MissingEnv {
                table: "[judge]",
                missing: vec!["GH_TOKEN".into(), "X".into()],
            }
        );
        assert_eq!(
            error.to_string(),
            "[judge].env declares GH_TOKEN, X, which the engine's environment does not set"
        );
    }

    /// The value is the engine's credential, so a Debug print of the resolved set names the
    /// variables and never their values.
    #[test]
    fn debug_names_the_variables_without_their_values() {
        let env = HostEnv::resolve_with("[world]", &names(&["TOKEN"]), |_| {
            Some(OsString::from("s3cret"))
        })
        .unwrap();
        let shown = format!("{env:?}");
        assert!(
            shown.contains("TOKEN") && !shown.contains("s3cret"),
            "{shown}"
        );
    }

    /// The command sees the base and what was declared; a variable the engine holds but the
    /// table did not declare is not there.
    #[test]
    fn a_command_sees_the_base_and_the_declared_variables_and_nothing_else() {
        let env = HostEnv::resolve_with("[workflow]", &names(&["DECLARED"]), |_| {
            Some(OsString::from("yes"))
        })
        .unwrap();
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("env");
        cmd.env("UNDECLARED", "leaked");
        env.apply(&mut cmd);
        let out = cmd.output().unwrap();
        let seen = String::from_utf8_lossy(&out.stdout);
        assert!(seen.lines().any(|l| l == "DECLARED=yes"), "{seen}");
        assert!(seen.lines().any(|l| l.starts_with("PATH=")), "{seen}");
        assert!(!seen.contains("UNDECLARED"), "{seen}");
    }
}

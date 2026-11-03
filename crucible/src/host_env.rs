//! The environment a pack-authored command runs with outside the sandbox (RFC-0001:C-COMMANDS):
//! a fixed base plus exactly the variables its manifest table declares. Nothing else of the
//! engine's environment reaches it, so a script the agent can edit cannot read the engine's own
//! credentials.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::process::Command;

/// What every such command receives from the engine's environment, when the engine has it.
pub const BASE: [&str; 4] = ["PATH", "HOME", "LANG", "TMPDIR"];

/// A variable a manifest table declares its commands receive.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
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

/// One entry of a table's `env` list: `NAME` is required, `NAME?` is passed through when the
/// engine's environment sets it and left out when it does not.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct EnvDecl {
    pub name: EnvName,
    pub required: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EnvDeclError {
    #[error("{entry:?} marks no variable optional; the name goes before the \"?\", as in \"X?\"")]
    NoName { entry: String },
    #[error("{entry:?} ends in more than one \"?\"; one marks the variable optional")]
    RepeatedMarker { entry: String },
    #[error("{entry:?} has a \"?\" inside the name; \"?\" may only end it, marking it optional")]
    MarkerInName { entry: String },
    #[error(transparent)]
    Name(#[from] EnvNameError),
}

impl TryFrom<String> for EnvDecl {
    type Error = EnvDeclError;

    fn try_from(entry: String) -> Result<Self, Self::Error> {
        let (name, required) = match entry.strip_suffix('?') {
            Some(name) => (name, false),
            None => (entry.as_str(), true),
        };
        if name.ends_with('?') {
            return Err(EnvDeclError::RepeatedMarker { entry });
        }
        if name.contains('?') {
            return Err(EnvDeclError::MarkerInName { entry });
        }
        if name.is_empty() && !required {
            return Err(EnvDeclError::NoName { entry });
        }
        let name = EnvName::try_from(name.to_owned())?;
        Ok(EnvDecl { name, required })
    }
}

impl From<EnvDecl> for String {
    fn from(decl: EnvDecl) -> Self {
        decl.to_string()
    }
}

impl fmt::Display for EnvDecl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.required {
            write!(f, "{}", self.name)
        } else {
            write!(f, "{}?", self.name)
        }
    }
}

/// A manifest table's `env` list. No name in it is both required and optional.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(try_from = "Vec<EnvDecl>", into = "Vec<EnvDecl>")]
pub struct DeclaredEnv(Vec<EnvDecl>);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("env lists both {name:?} and \"{name}?\"; a variable is required or optional, not both")]
pub struct RequiredAndOptional {
    name: String,
}

impl TryFrom<Vec<EnvDecl>> for DeclaredEnv {
    type Error = RequiredAndOptional;

    fn try_from(decls: Vec<EnvDecl>) -> Result<Self, Self::Error> {
        let mut required = BTreeMap::new();
        for decl in &decls {
            if *required.entry(&decl.name).or_insert(decl.required) != decl.required {
                return Err(RequiredAndOptional {
                    name: decl.name.to_string(),
                });
            }
        }
        Ok(DeclaredEnv(decls))
    }
}

impl From<DeclaredEnv> for Vec<EnvDecl> {
    fn from(declared: DeclaredEnv) -> Self {
        declared.0
    }
}

impl DeclaredEnv {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, EnvDecl> {
        self.0.iter()
    }
}

/// Required variables the engine's environment does not set. A command that depends on one
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
    pub fn resolve(table: &'static str, declared: &DeclaredEnv) -> Result<HostEnv, MissingEnv> {
        Self::resolve_with(table, declared, |name| std::env::var_os(name))
    }

    fn resolve_with(
        table: &'static str,
        declared: &DeclaredEnv,
        lookup: impl Fn(&str) -> Option<OsString>,
    ) -> Result<HostEnv, MissingEnv> {
        let mut found = Vec::with_capacity(declared.0.len());
        let mut missing = Vec::new();
        for decl in declared.iter() {
            match lookup(decl.name.as_str()) {
                Some(value) => found.push((decl.name.clone(), value)),
                None if decl.required => missing.push(decl.name.to_string()),
                None => {}
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
    use crate::host_env::{
        DeclaredEnv, EnvDecl, EnvDeclError, EnvName, HostEnv, MissingEnv, RequiredAndOptional,
    };
    use std::ffi::OsString;

    fn declared(entries: &[&str]) -> DeclaredEnv {
        let decls = entries
            .iter()
            .map(|e| EnvDecl::try_from((*e).to_owned()).unwrap())
            .collect::<Vec<_>>();
        DeclaredEnv::try_from(decls).unwrap()
    }

    fn env_of(env: &HostEnv) -> String {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("env");
        cmd.env("UNDECLARED", "leaked");
        env.apply(&mut cmd);
        let out = cmd.output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
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
    fn a_bare_name_is_required_and_a_trailing_question_mark_makes_it_optional() {
        let required = EnvDecl::try_from("KUBECONFIG".to_owned()).unwrap();
        assert_eq!(required.name.as_str(), "KUBECONFIG");
        assert!(required.required);
        let optional = EnvDecl::try_from("EPP_ENDPOINT?".to_owned()).unwrap();
        assert_eq!(optional.name.as_str(), "EPP_ENDPOINT");
        assert!(!optional.required);
        assert_eq!(String::from(required), "KUBECONFIG");
        assert_eq!(String::from(optional), "EPP_ENDPOINT?");
    }

    #[test]
    fn a_question_mark_with_no_name_before_it_is_refused_naming_the_entry() {
        let error = EnvDecl::try_from("?".to_owned()).unwrap_err();
        assert_eq!(error, EnvDeclError::NoName { entry: "?".into() });
        assert_eq!(
            error.to_string(),
            "\"?\" marks no variable optional; the name goes before the \"?\", as in \"X?\""
        );
    }

    #[test]
    fn more_than_one_trailing_question_mark_is_refused_naming_the_entry() {
        for entry in ["A??", "??", "A???"] {
            let error = EnvDecl::try_from(entry.to_owned()).unwrap_err();
            assert_eq!(
                error,
                EnvDeclError::RepeatedMarker {
                    entry: entry.into()
                }
            );
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("{entry:?} ends in more than one"))
            );
        }
    }

    #[test]
    fn a_question_mark_inside_the_name_is_refused_naming_the_entry() {
        for entry in ["A?B", "?A", "A?B?", "?A?"] {
            let error = EnvDecl::try_from(entry.to_owned()).unwrap_err();
            assert_eq!(
                error,
                EnvDeclError::MarkerInName {
                    entry: entry.into()
                }
            );
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("{entry:?} has a \"?\" inside the name")),
                "{error}"
            );
        }
    }

    #[test]
    fn an_optional_entry_still_needs_a_valid_name() {
        let error = EnvDecl::try_from("KUBE-CONFIG?".to_owned()).unwrap_err();
        assert!(matches!(error, EnvDeclError::Name(_)), "{error:?}");
        assert!(
            error
                .to_string()
                .starts_with("\"KUBE-CONFIG\" is not an environment variable name"),
            "{error}"
        );
    }

    #[test]
    fn a_name_listed_both_required_and_optional_is_refused_naming_both_entries() {
        for entries in [["GPUS", "GPUS?"], ["GPUS?", "GPUS"]] {
            let decls = entries
                .iter()
                .map(|e| EnvDecl::try_from((*e).to_owned()).unwrap())
                .collect::<Vec<_>>();
            let error = DeclaredEnv::try_from(decls).unwrap_err();
            assert_eq!(
                error,
                RequiredAndOptional {
                    name: "GPUS".into()
                }
            );
            assert_eq!(
                error.to_string(),
                "env lists both \"GPUS\" and \"GPUS?\"; a variable is required or optional, not both"
            );
        }
        assert_eq!(declared(&["GPUS", "GPUS"]).iter().count(), 2);
    }

    /// A manifest's `env` list reads and writes back in the manifest's own syntax.
    #[test]
    fn a_declared_list_round_trips_through_its_manifest_form() {
        let env: DeclaredEnv =
            serde_json::from_value(serde_json::json!(["KUBECONFIG", "EPP_ENDPOINT?"])).unwrap();
        assert_eq!(env, declared(&["KUBECONFIG", "EPP_ENDPOINT?"]));
        assert_eq!(
            serde_json::to_value(&env).unwrap(),
            serde_json::json!(["KUBECONFIG", "EPP_ENDPOINT?"])
        );
        let error = serde_json::from_value::<DeclaredEnv>(serde_json::json!(["A", "A?"]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("env lists both \"A\" and \"A?\""), "{error}");
    }

    #[test]
    fn every_declared_variable_the_engine_lacks_is_named_at_once() {
        let lookup = |name: &str| (name == "KUBECONFIG").then(|| OsString::from("/k"));
        let error = HostEnv::resolve_with(
            "[judge]",
            &declared(&["GH_TOKEN", "KUBECONFIG", "X"]),
            lookup,
        )
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

    /// An optional variable the engine lacks is no reason to refuse; a required one beside it
    /// still is, and the refusal names only the required one.
    #[test]
    fn a_required_variable_the_engine_lacks_refuses_and_an_optional_one_does_not() {
        let unset = |_: &str| None;
        let env = HostEnv::resolve_with("[world]", &declared(&["EPP_ENDPOINT?"]), unset).unwrap();
        assert_eq!(format!("{env:?}"), "[]");
        let error = HostEnv::resolve_with(
            "[world]",
            &declared(&["EPP_ENDPOINT?", "KUBECONFIG"]),
            unset,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "[world].env declares KUBECONFIG, which the engine's environment does not set"
        );
    }

    /// The value is the engine's credential, so a Debug print of the resolved set names the
    /// variables and never their values.
    #[test]
    fn debug_names_the_variables_without_their_values() {
        let env = HostEnv::resolve_with("[world]", &declared(&["TOKEN"]), |_| {
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
        let env = HostEnv::resolve_with("[workflow]", &declared(&["DECLARED"]), |_| {
            Some(OsString::from("yes"))
        })
        .unwrap();
        let seen = env_of(&env);
        assert!(seen.lines().any(|l| l == "DECLARED=yes"), "{seen}");
        assert!(seen.lines().any(|l| l.starts_with("PATH=")), "{seen}");
        assert!(!seen.contains("UNDECLARED"), "{seen}");
    }

    #[test]
    fn an_optional_variable_the_engine_sets_reaches_the_command() {
        let lookup = |name: &str| (name == "EPP_ENDPOINT").then(|| OsString::from("10.0.0.1:8000"));
        let env = HostEnv::resolve_with("[world]", &declared(&["EPP_ENDPOINT?"]), lookup).unwrap();
        let seen = env_of(&env);
        assert!(
            seen.lines().any(|l| l == "EPP_ENDPOINT=10.0.0.1:8000"),
            "{seen}"
        );
    }

    #[test]
    fn an_optional_variable_the_engine_lacks_is_absent_from_the_command() {
        let lookup = |name: &str| (name == "KUBECONFIG").then(|| OsString::from("/k"));
        let env = HostEnv::resolve_with(
            "[world]",
            &declared(&["KUBECONFIG", "EPP_ENDPOINT?"]),
            lookup,
        )
        .unwrap();
        let seen = env_of(&env);
        assert!(seen.lines().any(|l| l == "KUBECONFIG=/k"), "{seen}");
        assert!(!seen.contains("EPP_ENDPOINT"), "{seen}");
    }
}

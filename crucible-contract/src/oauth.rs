//! The OAuth scopes the controller mints for its MCP servers and each server checks.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    KueueRead,
    KueueSubmit,
    BuilditRead,
    BuilditBuild,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown scope `{0}`; one of kueue.read, kueue.submit, buildit.read, buildit.build")]
pub struct ScopeParseError(String);

impl Scope {
    /// Read queues, hardware, and workloads; submit, stop, resume, and delete workloads.
    pub const KUEUE: [Scope; 2] = [Scope::KueueRead, Scope::KueueSubmit];
    /// Read builds' status and logs; build, run, push, and clean.
    pub const BUILDIT: [Scope; 2] = [Scope::BuilditRead, Scope::BuilditBuild];

    pub const fn as_str(self) -> &'static str {
        match self {
            Scope::KueueRead => "kueue.read",
            Scope::KueueSubmit => "kueue.submit",
            Scope::BuilditRead => "buildit.read",
            Scope::BuilditBuild => "buildit.build",
        }
    }

    pub fn parse(s: &str) -> Result<Self, ScopeParseError> {
        match s {
            "kueue.read" => Ok(Scope::KueueRead),
            "kueue.submit" => Ok(Scope::KueueSubmit),
            "buildit.read" => Ok(Scope::BuilditRead),
            "buildit.build" => Ok(Scope::BuilditBuild),
            other => Err(ScopeParseError(other.to_string())),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Scope {
    type Err = ScopeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Scope::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use crate::oauth::*;

    #[test]
    fn every_scope_round_trips_through_its_wire_spelling() {
        for scope in Scope::KUEUE.into_iter().chain(Scope::BUILDIT) {
            assert_eq!(scope.as_str().parse::<Scope>(), Ok(scope));
            assert_eq!(scope.to_string(), scope.as_str());
        }
        assert_eq!(
            Scope::KUEUE.map(Scope::as_str),
            ["kueue.read", "kueue.submit"]
        );
        assert_eq!(
            Scope::BUILDIT.map(Scope::as_str),
            ["buildit.read", "buildit.build"]
        );
        assert!("kueue.admin".parse::<Scope>().is_err());
    }
}

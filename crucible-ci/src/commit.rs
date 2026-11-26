/// A full git commit identifier: 40 (SHA-1) or 64 (SHA-256) lowercase hexadecimal characters.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommitId(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0:?} is not a full commit id (40 or 64 lowercase hex characters)")]
pub struct NotACommit(pub String);

impl CommitId {
    pub fn parse(raw: &str) -> Result<Self, NotACommit> {
        let hex = raw
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        match hex && matches!(raw.len(), 40 | 64) {
            true => Ok(CommitId(raw.to_owned())),
            false => Err(NotACommit(raw.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn short(&self) -> &str {
        self.0.get(..12).unwrap_or(&self.0)
    }
}

impl std::fmt::Display for CommitId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use crate::commit::CommitId;

    #[test]
    fn only_full_lowercase_ids_parse() {
        let sha1 = "6a7acf93315f0000000000000000000000000000";
        assert_eq!(
            CommitId::parse(sha1).map(|c| c.short().to_owned()).ok(),
            Some("6a7acf93315f".to_owned())
        );
        assert!(CommitId::parse(&"a".repeat(64)).is_ok());
        assert!(CommitId::parse("6a7acf93315f").is_err());
        assert!(CommitId::parse(&"A".repeat(40)).is_err());
        assert!(CommitId::parse(&"a".repeat(41)).is_err());
        assert!(CommitId::parse(&"g".repeat(40)).is_err());
        assert!(CommitId::parse("").is_err());
    }
}

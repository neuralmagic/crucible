/// A bearer token for one CI system: the controller's bot token or the reader's own.
#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(String);

impl BearerToken {
    pub fn new(token: impl Into<String>) -> Self {
        BearerToken(token.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BearerToken(..)")
    }
}

#[cfg(test)]
mod tests {
    use crate::token::BearerToken;

    #[test]
    fn a_token_never_formats() {
        let token = BearerToken::new("sha256~secret");
        assert_eq!(format!("{token:?}"), "BearerToken(..)");
        assert_eq!(token.expose(), "sha256~secret");
    }
}

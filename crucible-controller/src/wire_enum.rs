//! The typed-error foundation for the controller's wire-vocabulary enums.
//!
//! A fieldless, total enum whose variants map one-to-one onto DB/CLI/query-string literals used to
//! hand-roll two parallel `match` tables plus an `anyhow::bail!("unknown …")` per enum. `wire_enum!`
//! generates both directions from a single `{ Variant => "lit" }` table and a typed [`ParseError`],
//! so the vocabulary lives once and a bad literal is a structured error, not an ad-hoc string.
//!
//! `ParseError` implements `std::error::Error` (via `thiserror`), so `?` in an `anyhow::Result` fn
//! absorbs it unchanged, and the API boundary's `.map_err(|e| e.to_string())` still reads its
//! `Display`.

#![allow(clippy::disallowed_macros)]

/// A wire/DB literal that matched no variant. `noun` names the vocabulary (`"issue status"`), so the
/// message reads `unknown issue status `frobnicate``.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ParseError {
    #[error("unknown {noun} `{value}`")]
    Unknown { noun: &'static str, value: String },
}

/// A fieldless enum with a total, bijective wire vocabulary: every variant maps to exactly one
/// literal and back. `NOUN` names the thing in a [`ParseError`].
pub(crate) trait WireEnum: Sized + Copy {
    const NOUN: &'static str;
    fn as_wire(self) -> &'static str;
    fn parse_wire(s: &str) -> Result<Self, ParseError>;
}

/// Generate a [`WireEnum`] impl from a `{ Variant => "literal" }` table, plus thin inherent
/// `parse`/`as_str` shims so existing call sites keep compiling.
///
/// - `both`: bidirectional — a stored `as_str` and a `parse`, plus sqlx `Type`/`Decode`/`Encode`
///   as TEXT, so a `FromRow` derive or `try_get` reads the enum straight off the row.
/// - `parse_only`: the enum is only ever parsed from the wire (query params); no stored `as_str`,
///   so it keeps whatever `Default`/helpers it already defines. Parsing at the API boundary goes
///   through [`parse_opt`]/[`parse_or_default`] (the trait's `parse_wire`); no inherent shims are
///   generated. The trait's `as_wire` still exists (it drives the round-trip test).
macro_rules! wire_enum {
    ($ty:ty, $noun:literal, both, { $($variant:path => $lit:literal),+ $(,)? }) => {
        wire_enum!(@trait $ty, $noun, { $($variant => $lit),+ });
        impl ::sqlx::Type<::sqlx::Postgres> for $ty {
            fn type_info() -> ::sqlx::postgres::PgTypeInfo {
                <::std::string::String as ::sqlx::Type<::sqlx::Postgres>>::type_info()
            }
            fn compatible(ty: &::sqlx::postgres::PgTypeInfo) -> bool {
                <::std::string::String as ::sqlx::Type<::sqlx::Postgres>>::compatible(ty)
            }
        }
        impl<'r> ::sqlx::Decode<'r, ::sqlx::Postgres> for $ty {
            fn decode(
                value: ::sqlx::postgres::PgValueRef<'r>,
            ) -> ::core::result::Result<Self, ::sqlx::error::BoxDynError> {
                let s = <&str as ::sqlx::Decode<'r, ::sqlx::Postgres>>::decode(value)?;
                ::core::result::Result::Ok(<Self as $crate::wire_enum::WireEnum>::parse_wire(s)?)
            }
        }
        impl<'q> ::sqlx::Encode<'q, ::sqlx::Postgres> for $ty {
            fn encode_by_ref(
                &self,
                buf: &mut ::sqlx::postgres::PgArgumentBuffer,
            ) -> ::core::result::Result<::sqlx::encode::IsNull, ::sqlx::error::BoxDynError> {
                <&str as ::sqlx::Encode<'q, ::sqlx::Postgres>>::encode_by_ref(
                    &<Self as $crate::wire_enum::WireEnum>::as_wire(*self),
                    buf,
                )
            }
        }
        impl $ty {
            /// The wire/DB spelling of this value.
            #[allow(dead_code)]
            pub(crate) fn as_str(self) -> &'static str {
                <Self as $crate::wire_enum::WireEnum>::as_wire(self)
            }
            /// Parse the wire/DB spelling; errors on anything outside the vocabulary rather than
            /// inventing a silent default (a bad literal is corruption, not a shrug).
            #[allow(dead_code)]
            pub(crate) fn parse(s: &str) -> ::core::result::Result<Self, $crate::wire_enum::ParseError> {
                <Self as $crate::wire_enum::WireEnum>::parse_wire(s)
            }
        }
    };
    ($ty:ty, $noun:literal, parse_only, { $($variant:path => $lit:literal),+ $(,)? }) => {
        wire_enum!(@trait $ty, $noun, { $($variant => $lit),+ });
    };
    (@trait $ty:ty, $noun:literal, { $($variant:path => $lit:literal),+ }) => {
        impl $crate::wire_enum::WireEnum for $ty {
            const NOUN: &'static str = $noun;
            fn as_wire(self) -> &'static str {
                match self { $($variant => $lit),+ }
            }
            fn parse_wire(s: &str) -> ::core::result::Result<Self, $crate::wire_enum::ParseError> {
                match s {
                    $($lit => ::core::result::Result::Ok($variant),)+
                    other => ::core::result::Result::Err($crate::wire_enum::ParseError::Unknown {
                        noun: Self::NOUN,
                        value: other.to_string(),
                    }),
                }
            }
        }
    };
}

pub(crate) use wire_enum;

/// Parse an optional wire-enum query param. A missing or empty value is absent (`None`); any other
/// value must parse or the caller gets the typed error's `Display` (a caller error, a 400). Empty is
/// absent because `?status=` is a client clearing the filter, not asserting a bad literal.
pub(crate) fn parse_opt<T: WireEnum>(s: Option<&str>) -> Result<Option<T>, String> {
    match s.filter(|v| !v.is_empty()) {
        Some(v) => Ok(Some(T::parse_wire(v).map_err(|e| e.to_string())?)),
        None => Ok(None),
    }
}

/// Parse a wire-enum query param that has a [`Default`]. A missing or empty value falls back to the
/// default; any other value must parse or the caller gets the typed error's `Display` (a 400).
pub(crate) fn parse_or_default<T: WireEnum + Default>(s: Option<&str>) -> Result<T, String> {
    match s.filter(|v| !v.is_empty()) {
        Some(v) => T::parse_wire(v).map_err(|e| e.to_string()),
        None => Ok(T::default()),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "autoresearch")]
    use super::{ParseError, WireEnum};
    #[cfg(feature = "autoresearch")]
    use crate::builds::model::{BuildBackendKind, BuildState};
    #[cfg(feature = "autoresearch")]
    use crate::issues::model::{SortKey, UpstreamState};
    #[cfg(feature = "autoresearch")]
    use crate::issues::ranker::{Affinity, Confidence};
    #[cfg(feature = "autoresearch")]
    use crate::launches::model::OneShotStatus;
    #[cfg(feature = "autoresearch")]
    use crate::model::LaunchOrigin;
    #[cfg(feature = "autoresearch")]
    use crate::model::SortDir;
    #[cfg(feature = "autoresearch")]
    use crate::model::{ParkedBy, Status, Trigger};
    #[cfg(feature = "autoresearch")]
    use crate::playbooks::imports::ImportStatus;
    #[cfg(feature = "autoresearch")]
    use crate::playbooks::providers::{DefaultScope, ProviderKind, WorkloadClass};
    #[cfg(feature = "autoresearch")]
    use crate::runs::model::{RunKindFilter, RunSort};
    #[cfg(feature = "autoresearch")]
    use crate::runs::workpod::WorkPodState;
    #[cfg(feature = "autoresearch")]
    use crate::secrets::{
        AuditAction, ConsumerClass, ProjectionKind, ScopeKind, SecretKind, SecretMode, Visibility,
    };
    #[cfg(feature = "autoresearch")]
    use strum::IntoEnumIterator;

    /// `parse_wire(as_wire(v)) == v` for every variant (enumerated via `EnumIter`, so a new variant
    /// is covered automatically), and an unknown literal is a typed error naming the vocabulary.
    #[cfg(feature = "autoresearch")]
    fn assert_round_trip<T>()
    where
        T: WireEnum + IntoEnumIterator + PartialEq + std::fmt::Debug,
    {
        for v in T::iter() {
            let back = T::parse_wire(T::as_wire(v)).expect("as_wire spelling must parse back");
            assert_eq!(back, v);
        }
        let err = T::parse_wire("__definitely_not_a_variant__").expect_err("unknown must error");
        let ParseError::Unknown { noun, value } = err;
        assert_eq!(noun, T::NOUN);
        assert_eq!(value, "__definitely_not_a_variant__");
    }

    #[cfg(feature = "autoresearch")]
    #[test]
    fn every_wire_enum_round_trips() {
        assert_round_trip::<Status>();
        assert_round_trip::<ParkedBy>();
        assert_round_trip::<UpstreamState>();
        assert_round_trip::<BuildState>();
        assert_round_trip::<BuildBackendKind>();
        assert_round_trip::<WorkPodState>();
        assert_round_trip::<Confidence>();
        assert_round_trip::<Affinity>();
        assert_round_trip::<SortKey>();
        assert_round_trip::<SortDir>();
        assert_round_trip::<RunSort>();
        assert_round_trip::<RunKindFilter>();
        assert_round_trip::<LaunchOrigin>();
        assert_round_trip::<Trigger>();
        assert_round_trip::<OneShotStatus>();
        assert_round_trip::<SecretKind>();
        assert_round_trip::<Visibility>();
        assert_round_trip::<ConsumerClass>();
        assert_round_trip::<SecretMode>();
        assert_round_trip::<ScopeKind>();
        assert_round_trip::<ProjectionKind>();
        assert_round_trip::<AuditAction>();
        assert_round_trip::<ProviderKind>();
        assert_round_trip::<WorkloadClass>();
        assert_round_trip::<DefaultScope>();
        assert_round_trip::<ImportStatus>();
    }
}

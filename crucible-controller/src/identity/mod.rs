//! Identity: who a caller is and what they may do. The auth middleware and its role model, the
//! cookie session, the native OIDC login, API keys, the kube user probe, and per-user preferences.

pub(crate) mod api;
pub mod api_key;
pub mod auth;
pub mod kube_user;
pub mod oidc;
pub mod session;
pub mod user_prefs;

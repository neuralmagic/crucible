//! The identity slice's HTTP handlers, mounted by [`crate::api`].

pub(crate) mod cluster_identities;
pub(crate) mod consent;
pub(crate) mod credentials;
pub(crate) mod keys;
pub(crate) mod kube_user;
pub(crate) mod prefs;

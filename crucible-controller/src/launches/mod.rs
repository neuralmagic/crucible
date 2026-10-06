//! Launches: the standing authorization to run a playbook without a session, and the triggers
//! that fire it (a cron schedule, a one-shot, a tracker watch, a webhook), with the tracker clients and the
//! review-trail emission that follow a launch.

pub(crate) mod api;
pub mod emission;
pub mod jira;
pub mod model;
pub mod one_shots;
pub mod pending;
pub mod schedules;
pub mod standing;
pub mod store;
pub mod tracker;
pub mod watches;
pub mod webhooks;

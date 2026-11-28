//! What the app does with its state, one concern to a file. Each adds methods to [`crate::App`].

pub mod boot;
#[cfg(not(target_arch = "wasm32"))]
mod forge;
mod launching;
mod opening;
mod pages;
#[cfg(not(target_arch = "wasm32"))]
pub mod promote;
mod reports;
mod selection;
mod signing;
mod syncing;

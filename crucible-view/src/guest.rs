//! The functions behind the exports `plugin!` writes, built for `wasm32-unknown-unknown`.
//!
//! `view` answers both entries: `crucible_view` reads a [`crate::Request`] for one run, and
//! `crucible_runs_view`, written when `plugin!` names a `runs_view`, reads a
//! [`crate::RunsRequest`] for the launches of the playbook. `inputs` answers `crucible_inputs`
//! and `crucible_runs_inputs`.
//!
//! A module keeps nothing between calls the host can rely on, and has no clock: a live view
//! keeps what it needs in [`crate::Live::state`], gets it back as the request's `state`, and is
//! told what happened in its `event`.
//!
//! A result is returned as one `u64`: its address in the high half, its length in the low half.

use serde::de::DeserializeOwned;

use crate::{Reply, View};

pub fn alloc(len: u32) -> u32 {
    let mut bytes = Vec::<u8>::with_capacity(len as usize);
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    ptr as u32
}

pub fn inputs(paths: &[&str]) -> u64 {
    hand_back(serde_json::to_vec(paths).unwrap_or_default())
}

pub fn view<R: DeserializeOwned>(ptr: u32, len: u32, build: fn(R) -> Result<View, String>) -> u64 {
    // SAFETY: the host wrote `len` bytes at the address `alloc(len)` returned.
    let bytes = unsafe { Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize) };
    let reply = match serde_json::from_slice::<R>(&bytes) {
        Ok(request) => match build(request) {
            Ok(view) => Reply::View(view),
            Err(error) => Reply::Error(error),
        },
        Err(error) => Reply::Error(format!("reading the request: {error}")),
    };
    hand_back(serde_json::to_vec(&reply).unwrap_or_default())
}

fn hand_back(bytes: Vec<u8>) -> u64 {
    let bytes = bytes.into_boxed_slice();
    let len = bytes.len() as u64;
    let ptr = Box::into_raw(bytes) as *mut u8 as u32;
    (u64::from(ptr) << 32) | len
}

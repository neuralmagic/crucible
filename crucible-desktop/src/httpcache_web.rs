//! The browser build keeps no response cache: the browser's own HTTP cache stands in.

use std::path::PathBuf;

use crate::search::Hit;

pub fn cache_root() -> Option<PathBuf> {
    None
}

pub enum Store {}

impl Store {
    pub fn search(&self, _base: &str, _playbook: &str, _query: &str, _limit: usize) -> Vec<Hit> {
        match *self {}
    }

    pub fn get(&self, _base: &str, _path: &str) -> Option<(Vec<u8>, bool)> {
        match *self {}
    }

    pub fn indexed(&self, _base: &str, _path: &str) -> bool {
        match *self {}
    }
}

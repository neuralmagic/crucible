//! The image catalog: the sandbox images a pack may pick, read off configured registries and
//! cached per digest with their capability documents.
pub(crate) mod api;
pub mod model;
pub mod registry;
pub(crate) mod store;
pub mod sweep;

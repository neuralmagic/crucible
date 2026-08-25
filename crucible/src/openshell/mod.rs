//! In-Rust OpenShell backend: run a sandboxed agent turn against the local OpenShell gateway.
//! The control-plane boundary is the gateway's native gRPC API ([`grpc`]), typed Health /
//! sandbox / provider / policy / exec / logs RPCs. Two things stay on the `openshell` CLI
//! because no RPC covers them: file upload/download (SSH-tar) and the gateway/podman boot.
//!
//! Module map:
//! - [`grpc`]: the tonic channel, mTLS setup, and the typed per-turn gateway client.
//! - `provider`: mint the Vertex token from ADC (`gcp_auth`) per turn (constants + minting).
//! - `policy`: resolve the egress allowlist (endpoints + binaries) from the manifest.
//! - `sandbox`: the per-workspace sandbox name + the surviving CLI upload/download argv.
//! - `gateway`: boot/teardown the local gateway + rootless podman socket (still subprocess).
//! - `run`: the per-turn flow: env script, prompt-over-stdin, exec, download.

pub mod gateway;
pub mod grpc;
pub mod policy;

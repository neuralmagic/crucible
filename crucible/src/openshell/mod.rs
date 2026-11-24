//! In-Rust OpenShell backend: run a sandboxed agent turn against the local OpenShell gateway.
//! The control-plane boundary is the gateway's native gRPC API ([`grpc`]), typed Health /
//! sandbox / provider / policy / exec / logs RPCs. Two things stay on the `openshell` CLI
//! because no RPC covers them: file upload/download (SSH-tar) and the gateway/podman boot.
//!
//! Module map:
//! - [`grpc`]: the tonic channel, mTLS setup, and the typed per-turn gateway client.
//! - [`placement`]: where a sandbox that asks for GPUs is scheduled.
//! - `policy`: resolve the egress allowlist (endpoints + binaries) from the manifest.
//! - `gateway`: boot/teardown the local gateway + rootless podman socket (still subprocess).

pub mod gateway;
pub mod grpc;
pub mod placement;
pub mod policy;

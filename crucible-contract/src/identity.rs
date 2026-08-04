//! `RunIdentity`: the comparability key two runs must share for their scores to be comparable.
//!
//! Pure structs + the digest primitive only, with no manifest dependency. Building a [`RunIdentity`]
//! from a loaded manifest lives in `crucible::identity`, which calls [`RunIdentity::new`] here to
//! compute the digest.

use serde::{Deserialize, Serialize};

/// Bump when the digest's input list changes shape, so an old digest can never silently collide
/// with a new one over a different set of inputs. v2 added `seed_hash`.
const FORMAT_VERSION: &str = "v2";

/// One component's contribution to the identity: where its source came from and the pristine
/// commit the workspace started from. A single-domain run has exactly one (unnamed) entry; a
/// composite run has one per `[[component]]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComponentIdentity {
    /// The component's domain name (empty for a single-domain run, where there is nothing to
    /// disambiguate).
    pub name: String,
    /// `[repo].url` or `[repo].path`, whichever the manifest set.
    pub repo: String,
    /// The pristine commit SHA the component's workspace was checked out at, before any agent
    /// commit.
    pub base_sha: String,
}

/// The deployment (`[rig]`) definition ref and the backend name+version join the comparability
/// tuple. Every field is empty until the `[rig]` manifest section lands; the struct is modeled now
/// so the tuple's shape (and its digest inputs) don't change out from under existing digests when
/// it does.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RigIdentity {
    #[serde(default)]
    def_ref: String,
    #[serde(default)]
    backend_name: String,
    #[serde(default)]
    backend_version: String,
}

/// The comparability key. Two runs are comparable only if every field here matches (`digest` is
/// the cheap hash-of-hashes; the rest is carried alongside so a human can diff *why* two runs
/// differ instead of just learning that they do).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunIdentity {
    pub components: Vec<ComponentIdentity>,
    /// Hash of the frozen manifest text actually loaded (see `manifest::frozen_manifest_text`).
    pub manifest_hash: String,
    /// Hash over each `[[workspace.inject]]` entry's source content + destination path.
    pub inject_hash: String,
    /// Hash of the `[agent].seed_diff` content, the pack-declared starting diff handed to
    /// iteration 1. Empty when the manifest declares none. Separate from `inject_hash` so a run
    /// seeded with a known-good diff can never share a digest with an unseeded one (run 6
    /// published a hand-steered diff under an identity that said otherwise).
    #[serde(default)]
    pub seed_hash: String,
    /// `[judge].measure_cmd`, the gate's identity.
    pub measure_cmd: String,
    /// `[judge].direction` (`"lower"`/`"higher"`), raw from the manifest.
    pub direction: String,
    #[serde(default)]
    pub rig: RigIdentity,
    /// `v1:<hex>`, the hash-of-hashes over every field above, in fixed order.
    pub digest: String,
}

impl RunIdentity {
    pub fn new(
        components: Vec<ComponentIdentity>,
        manifest_hash: String,
        inject_hash: String,
        seed_hash: String,
        measure_cmd: String,
        direction: String,
        rig: RigIdentity,
    ) -> Self {
        let digest = compute_digest(
            &components,
            &manifest_hash,
            &inject_hash,
            &seed_hash,
            &measure_cmd,
            &direction,
            &rig,
        );
        Self {
            components,
            manifest_hash,
            inject_hash,
            seed_hash,
            measure_cmd,
            direction,
            rig,
            digest,
        }
    }
}

fn compute_digest(
    components: &[ComponentIdentity],
    manifest_hash: &str,
    inject_hash: &str,
    seed_hash: &str,
    measure_cmd: &str,
    direction: &str,
    rig: &RigIdentity,
) -> String {
    let mut parts: Vec<&[u8]> = Vec::new();
    for c in components {
        parts.push(c.name.as_bytes());
        parts.push(c.repo.as_bytes());
        parts.push(c.base_sha.as_bytes());
    }
    parts.push(manifest_hash.as_bytes());
    parts.push(inject_hash.as_bytes());
    parts.push(seed_hash.as_bytes());
    parts.push(measure_cmd.as_bytes());
    parts.push(direction.as_bytes());
    parts.push(rig.def_ref.as_bytes());
    parts.push(rig.backend_name.as_bytes());
    parts.push(rig.backend_version.as_bytes());
    format!("{FORMAT_VERSION}:{}", fnv1a_hex(&parts))
}

/// FNV-1a over a sequence of byte slices, each terminated by a `0x01` separator so `["ab","c"]`
/// and `["a","bc"]` can't collide.
pub fn fnv1a_hex(parts: &[&[u8]]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in *part {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= 0x01;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

//! Signed provenance for the commits a run pushes.
//!
//! The controller signs one in-toto statement per pushed commit, carried in a DSSE envelope.
//! Signing and verification live here so the controller that signs and the `crux verify` that
//! checks compute the same pre-authentication encoding, key id, and envelope digest.
//!
//! ```text
//! envelope bytes ──sha256──> EnvelopeDigest   (the opaque public reference)
//!      │
//!      └─ payloadType + base64(statement JSON) + [{keyid, sig}]
//!                               │                    │
//!                               │      ed25519 over PAE(payloadType, statement JSON)
//!                               v
//!          subject: {name: repository, digest: {gitCommit: sha}}
//!          predicate: what the controller recorded about the run that pushed it
//! ```

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::link::{ExternalLink, LinkKind};

/// The env var the controller sets on a run pod to the JSON [`DispatchProvenance`] it resolved.
pub const ENV_PROVENANCE: &str = "CRUCIBLE_PROVENANCE";

pub const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
pub const PREDICATE_TYPE: &str = "https://github.com/neuralmagic/crucible/provenance/commit/v1";
pub const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
pub const ALGORITHM: &str = "ed25519";

/// What the controller resolved at dispatch that the engine cannot see from inside the run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchProvenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The sandbox image, by digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_image: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProvenanceError {
    #[error("{0:?} is not a full commit sha (40 or 64 hex characters)")]
    CommitSha(String),
    #[error("{0:?} is not an envelope digest (sha256:<64 hex characters>)")]
    EnvelopeDigest(String),
    #[error("{0:?} is not a key id (64 hex characters)")]
    KeyId(String),
    #[error("reading the signing key: {0}")]
    SigningKey(String),
    #[error("encoding the statement: {0}")]
    Encode(String),
}

/// A full git object id, lowercase.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommitSha(String);

impl CommitSha {
    pub fn parse(value: &str) -> Result<Self, ProvenanceError> {
        let lower = value.to_ascii_lowercase();
        match is_hex(&lower) && matches!(lower.len(), 40 | 64) {
            true => Ok(Self(lower)),
            false => Err(ProvenanceError::CommitSha(value.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CommitSha {
    type Error = ProvenanceError;
    fn try_from(value: String) -> Result<Self, ProvenanceError> {
        Self::parse(&value)
    }
}

impl From<CommitSha> for String {
    fn from(value: CommitSha) -> String {
        value.0
    }
}

impl std::fmt::Display for CommitSha {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `sha256:<hex>` over an envelope's exact stored bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EnvelopeDigest(String);

impl EnvelopeDigest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(crate::artifact::content_digest(bytes))
    }

    pub fn parse(value: &str) -> Result<Self, ProvenanceError> {
        match value.strip_prefix("sha256:") {
            Some(hex) if hex.len() == 64 && is_hex(hex) => Ok(Self(value.to_owned())),
            _ => Err(ProvenanceError::EnvelopeDigest(value.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EnvelopeDigest {
    type Error = ProvenanceError;
    fn try_from(value: String) -> Result<Self, ProvenanceError> {
        Self::parse(&value)
    }
}

impl From<EnvelopeDigest> for String {
    fn from(value: EnvelopeDigest) -> String {
        value.0
    }
}

impl std::fmt::Display for EnvelopeDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The lowercase hex SHA-256 of a raw ed25519 public key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct KeyId(String);

impl KeyId {
    pub fn of(key: &VerifyingKey) -> Self {
        Self(crate::artifact::sha256_hex(key.as_bytes()))
    }

    pub fn parse(value: &str) -> Result<Self, ProvenanceError> {
        match value.len() == 64 && is_hex(value) {
            true => Ok(Self(value.to_owned())),
            false => Err(ProvenanceError::KeyId(value.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for KeyId {
    type Error = ProvenanceError;
    fn try_from(value: String) -> Result<Self, ProvenanceError> {
        Self::parse(&value)
    }
}

impl From<KeyId> for String {
    fn from(value: KeyId) -> String {
        value.0
    }
}

impl std::fmt::Display for KeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn is_hex(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A commit a task reported pushing: a commit link naming the repository and the full sha.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PushedCommit {
    /// `https://<host>/<project path>`, the repository the commit link names.
    pub repository: String,
    pub sha: CommitSha,
}

impl PushedCommit {
    /// The commit a GitHub (`/<owner>/<repo>/commit/<sha>`) or GitLab
    /// (`/<group…>/<project>/-/commit/<sha>`) commit link names. `None` for any other link and
    /// for an abbreviated sha, which names no one commit.
    pub fn from_link(link: &ExternalLink) -> Option<Self> {
        if link.kind != LinkKind::Commit {
            return None;
        }
        let url = url::Url::parse(link.url.as_str()).ok()?;
        let host = url.host_str()?;
        let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        let at = segments.iter().position(|s| *s == "commit")?;
        let project = match segments[..at] {
            [.., "-"] => &segments[..at - 1],
            _ => &segments[..at],
        };
        let sha = CommitSha::parse(segments.get(at + 1)?).ok()?;
        (!project.is_empty()).then(|| PushedCommit {
            repository: format!("https://{host}/{}", project.join("/")),
            sha,
        })
    }

    /// Every distinct commit `links` reports, in order.
    pub fn from_links(links: &[ExternalLink]) -> Vec<Self> {
        let mut commits: Vec<Self> = Vec::new();
        for commit in links.iter().filter_map(Self::from_link) {
            if !commits.contains(&commit) {
                commits.push(commit);
            }
        }
        commits
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Statement {
    #[serde(rename = "_type")]
    pub statement_type: String,
    pub subject: Vec<Subject>,
    #[serde(rename = "predicateType")]
    pub predicate_type: String,
    pub predicate: CommitPredicate,
}

impl Statement {
    pub fn new(commit: &PushedCommit, predicate: CommitPredicate) -> Self {
        Statement {
            statement_type: STATEMENT_TYPE.to_owned(),
            subject: vec![Subject {
                name: commit.repository.clone(),
                digest: SubjectDigest {
                    git_commit: commit.sha.clone(),
                },
            }],
            predicate_type: PREDICATE_TYPE.to_owned(),
            predicate,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subject {
    pub name: String,
    pub digest: SubjectDigest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubjectDigest {
    #[serde(rename = "gitCommit")]
    pub git_commit: CommitSha,
}

/// What the controller recorded about the run and task that pushed the subject. A field the
/// controller has no record of is absent, never guessed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommitPredicate {
    pub run: String,
    pub task: String,
    pub iter: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playbook: Option<PlaybookRef>,
    /// The digest of the admitted plan graph the task ran in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// The digest of the parameter schema the launch was validated against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// The agent tasks at or upstream of the pushing task, with what the engine ran them on.
    #[serde(default)]
    pub agents: Vec<AgentRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<String>,
    /// The command and evaluate tasks upstream of the pushing task, as the engine settled them.
    #[serde(default)]
    pub checks: Vec<CheckResult>,
    pub signed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookRef {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_version: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub task: String,
    pub harness: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub task: String,
    pub status: String,
}

/// A DSSE envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsseEnvelope {
    #[serde(rename = "payloadType")]
    pub payload_type: String,
    /// Standard base64 of the statement JSON.
    pub payload: String,
    pub signatures: Vec<DsseSignature>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsseSignature {
    pub keyid: String,
    /// Standard base64 of the raw signature.
    pub sig: String,
}

/// DSSE's pre-authentication encoding: what a signature actually covers.
pub fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "DSSEv1 {} {payload_type} {} ",
        payload_type.len(),
        payload.len()
    )
    .into_bytes();
    out.extend_from_slice(payload);
    out
}

/// One public key a verifier trusts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicKey {
    pub keyid: KeyId,
    pub algorithm: String,
    /// Standard base64 of the raw 32-byte ed25519 public key.
    pub public_key: String,
}

impl PublicKey {
    pub fn of(key: &VerifyingKey) -> Self {
        PublicKey {
            keyid: KeyId::of(key),
            algorithm: ALGORITHM.to_owned(),
            public_key: STANDARD.encode(key.as_bytes()),
        }
    }

    /// The key, if it is an ed25519 key whose bytes hash to its own key id.
    fn verifying_key(&self) -> Option<VerifyingKey> {
        if self.algorithm != ALGORITHM {
            return None;
        }
        let raw: [u8; 32] = STANDARD.decode(&self.public_key).ok()?.try_into().ok()?;
        let key = VerifyingKey::from_bytes(&raw).ok()?;
        (KeyId::of(&key) == self.keyid).then_some(key)
    }
}

/// The keys statements verify against.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeySet {
    pub keys: Vec<PublicKey>,
}

/// The controller's signing key.
pub struct Signer {
    key: SigningKey,
    keyid: KeyId,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("keyid", &self.keyid)
            .finish()
    }
}

/// A signed envelope: the bytes to store and serve, and their digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedEnvelope {
    pub digest: EnvelopeDigest,
    pub bytes: Vec<u8>,
}

impl Signer {
    /// A PKCS#8 PEM ed25519 private key, as `openssl genpkey -algorithm ed25519` writes it.
    pub fn from_pkcs8_pem(pem: &str) -> Result<Self, ProvenanceError> {
        SigningKey::from_pkcs8_pem(pem)
            .map(Self::new)
            .map_err(|e| ProvenanceError::SigningKey(e.to_string()))
    }

    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self::new(SigningKey::from_bytes(seed))
    }

    fn new(key: SigningKey) -> Self {
        let keyid = KeyId::of(&key.verifying_key());
        Signer { key, keyid }
    }

    pub fn keyid(&self) -> &KeyId {
        &self.keyid
    }

    pub fn public_key(&self) -> PublicKey {
        PublicKey::of(&self.key.verifying_key())
    }

    pub fn key_set(&self) -> KeySet {
        KeySet {
            keys: vec![self.public_key()],
        }
    }

    pub fn sign(&self, statement: &Statement) -> Result<SignedEnvelope, ProvenanceError> {
        let payload =
            serde_json::to_vec(statement).map_err(|e| ProvenanceError::Encode(e.to_string()))?;
        let sig = self.key.sign(&pae(PAYLOAD_TYPE, &payload));
        let envelope = DsseEnvelope {
            payload_type: PAYLOAD_TYPE.to_owned(),
            payload: STANDARD.encode(&payload),
            signatures: vec![DsseSignature {
                keyid: self.keyid.to_string(),
                sig: STANDARD.encode(sig.to_bytes()),
            }],
        };
        let bytes =
            serde_json::to_vec(&envelope).map_err(|e| ProvenanceError::Encode(e.to_string()))?;
        Ok(SignedEnvelope {
            digest: EnvelopeDigest::of(&bytes),
            bytes,
        })
    }
}

/// What a verification checks the envelope against, beyond its own signature.
#[derive(Debug, Clone, Copy, Default)]
pub struct Expect<'a> {
    pub digest: Option<&'a EnvelopeDigest>,
    pub commit: Option<&'a CommitSha>,
    pub repository: Option<&'a str>,
}

/// The verification steps, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyStep {
    Digest,
    Envelope,
    PayloadType,
    KeyId,
    Signature,
    Statement,
    Subject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{step:?}: {detail}")]
pub struct VerifyFailure {
    pub step: VerifyStep,
    pub detail: String,
}

fn fail(step: VerifyStep, detail: impl Into<String>) -> VerifyFailure {
    VerifyFailure {
        step,
        detail: detail.into(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verified {
    pub digest: EnvelopeDigest,
    pub keyid: KeyId,
    pub statement: Statement,
}

/// Check an envelope's stored bytes: digest, payload type, a known key id, the signature over
/// the pre-authentication encoding, the statement and predicate types, and the subject.
pub fn verify(bytes: &[u8], keys: &KeySet, expect: Expect<'_>) -> Result<Verified, VerifyFailure> {
    let digest = EnvelopeDigest::of(bytes);
    if let Some(want) = expect.digest
        && *want != digest
    {
        return Err(fail(
            VerifyStep::Digest,
            format!("the envelope hashes to {digest}, not {want}"),
        ));
    }
    let envelope: DsseEnvelope = serde_json::from_slice(bytes)
        .map_err(|e| fail(VerifyStep::Envelope, format!("not a DSSE envelope: {e}")))?;
    if envelope.payload_type != PAYLOAD_TYPE {
        return Err(fail(
            VerifyStep::PayloadType,
            format!(
                "payload type is {:?}, not {PAYLOAD_TYPE:?}",
                envelope.payload_type
            ),
        ));
    }
    let payload = STANDARD
        .decode(&envelope.payload)
        .map_err(|e| fail(VerifyStep::Envelope, format!("payload is not base64: {e}")))?;
    let signed = envelope
        .signatures
        .iter()
        .filter_map(|s| {
            let key = keys.keys.iter().find(|k| k.keyid.as_str() == s.keyid)?;
            Some((s, key))
        })
        .collect::<Vec<_>>();
    if signed.is_empty() {
        return Err(fail(
            VerifyStep::KeyId,
            "no signature names a key in the key set",
        ));
    }
    let message = pae(&envelope.payload_type, &payload);
    let keyid = signed
        .iter()
        .find_map(|(s, key)| {
            let verifying = key.verifying_key()?;
            let raw: [u8; 64] = STANDARD.decode(&s.sig).ok()?.try_into().ok()?;
            let sig = ed25519_dalek::Signature::from_bytes(&raw);
            verifying
                .verify_strict(&message, &sig)
                .ok()
                .map(|()| key.keyid.clone())
        })
        .ok_or_else(|| fail(VerifyStep::Signature, "no signature verifies under its key"))?;
    let statement: Statement = serde_json::from_slice(&payload)
        .map_err(|e| fail(VerifyStep::Statement, format!("not a statement: {e}")))?;
    if statement.statement_type != STATEMENT_TYPE || statement.predicate_type != PREDICATE_TYPE {
        return Err(fail(
            VerifyStep::Statement,
            format!(
                "statement is {:?} with predicate {:?}",
                statement.statement_type, statement.predicate_type
            ),
        ));
    }
    let [subject] = statement.subject.as_slice() else {
        return Err(fail(
            VerifyStep::Subject,
            format!("{} subjects, not one", statement.subject.len()),
        ));
    };
    if let Some(want) = expect.commit
        && subject.digest.git_commit != *want
    {
        return Err(fail(
            VerifyStep::Subject,
            format!("subject is {}, not {want}", subject.digest.git_commit),
        ));
    }
    if let Some(want) = expect.repository
        && subject.name != want
    {
        return Err(fail(
            VerifyStep::Subject,
            format!("subject repository is {}, not {want}", subject.name),
        ));
    }
    Ok(Verified {
        digest,
        keyid,
        statement,
    })
}

#[cfg(test)]
mod tests {
    use crate::provenance::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn commit() -> PushedCommit {
        PushedCommit {
            repository: "https://github.com/acme/widget".into(),
            sha: CommitSha::parse(SHA).unwrap(),
        }
    }

    fn predicate() -> CommitPredicate {
        CommitPredicate {
            run: "20261027T000000Z-a".into(),
            task: "push".into(),
            iter: 0,
            playbook: Some(PlaybookRef {
                id: "bumps".into(),
                draft_version: None,
            }),
            plan: Some(format!("sha256:{}", "a".repeat(64))),
            schema: None,
            agents: vec![AgentRecord {
                task: "author".into(),
                harness: "claude".into(),
                model: "m".into(),
            }],
            provider: Some("p".into()),
            sandbox_image: Some(format!(
                "registry.example/sandbox@sha256:{}",
                "b".repeat(64)
            )),
            cluster: Some("east".into()),
            pull_request: Some("https://github.com/acme/widget/pull/7".into()),
            checks: vec![CheckResult {
                task: "build".into(),
                status: "pass".into(),
            }],
            signed_at: "2026-10-27T00:00:00Z".into(),
        }
    }

    fn signed() -> (Signer, SignedEnvelope) {
        let signer = Signer::from_seed(&[7; 32]);
        let env = signer
            .sign(&Statement::new(&commit(), predicate()))
            .unwrap();
        (signer, env)
    }

    fn reenvelope(bytes: &[u8], edit: impl FnOnce(&mut DsseEnvelope)) -> Vec<u8> {
        let mut env: DsseEnvelope = serde_json::from_slice(bytes).unwrap();
        edit(&mut env);
        serde_json::to_vec(&env).unwrap()
    }

    #[test]
    fn a_signed_envelope_verifies_against_its_key() {
        let (signer, env) = signed();
        let sha = CommitSha::parse(SHA).unwrap();
        let ok = verify(
            &env.bytes,
            &signer.key_set(),
            Expect {
                digest: Some(&env.digest),
                commit: Some(&sha),
                repository: Some("https://github.com/acme/widget"),
            },
        )
        .unwrap();
        assert_eq!(ok.digest, env.digest);
        assert_eq!(&ok.keyid, signer.keyid());
        assert_eq!(ok.statement.predicate, predicate());
        assert_eq!(ok.statement.subject[0].digest.git_commit, sha);
    }

    #[test]
    fn signing_is_deterministic_for_the_same_statement() {
        let (_, a) = signed();
        let (_, b) = signed();
        assert_eq!(a, b);
    }

    #[test]
    fn pae_matches_the_dsse_spec() {
        assert_eq!(
            pae("http://example.com/HelloWorld", b"hello world"),
            b"DSSEv1 29 http://example.com/HelloWorld 11 hello world".to_vec()
        );
    }

    #[test]
    fn the_key_id_is_the_sha256_of_the_raw_public_key() {
        let signer = Signer::from_seed(&[7; 32]);
        let raw = STANDARD.decode(signer.public_key().public_key).unwrap();
        assert_eq!(raw.len(), 32);
        assert_eq!(
            signer.keyid().as_str(),
            crate::artifact::sha256_hex(&raw).as_str()
        );
    }

    #[test]
    fn every_step_refuses_its_own_tampering() {
        let (signer, env) = signed();
        let keys = signer.key_set();
        let sha = CommitSha::parse(SHA).unwrap();
        let step =
            |bytes: &[u8], expect: Expect<'_>| verify(bytes, &keys, expect).unwrap_err().step;

        let mut flipped = env.bytes.clone();
        let last = flipped.len() - 2;
        flipped[last] ^= 1;
        assert_eq!(
            step(
                &flipped,
                Expect {
                    digest: Some(&env.digest),
                    ..Expect::default()
                }
            ),
            VerifyStep::Digest
        );
        assert_eq!(step(b"not json", Expect::default()), VerifyStep::Envelope);

        let other_type = reenvelope(&env.bytes, |e| e.payload_type = "text/plain".into());
        assert_eq!(
            step(&other_type, Expect::default()),
            VerifyStep::PayloadType
        );

        let unknown_key = reenvelope(&env.bytes, |e| e.signatures[0].keyid = "c".repeat(64));
        assert_eq!(step(&unknown_key, Expect::default()), VerifyStep::KeyId);

        let mut statement = Statement::new(&commit(), predicate());
        statement.predicate.agents[0].model = "a model the run never used".into();
        let swapped_payload = reenvelope(&env.bytes, |e| {
            e.payload = STANDARD.encode(serde_json::to_vec(&statement).unwrap())
        });
        assert_eq!(
            step(&swapped_payload, Expect::default()),
            VerifyStep::Signature
        );

        let other = CommitSha::parse(&"f".repeat(40)).unwrap();
        assert_eq!(
            step(
                &env.bytes,
                Expect {
                    commit: Some(&other),
                    ..Expect::default()
                }
            ),
            VerifyStep::Subject
        );
        assert_eq!(
            step(
                &env.bytes,
                Expect {
                    commit: Some(&sha),
                    repository: Some("https://github.com/acme/other"),
                    ..Expect::default()
                }
            ),
            VerifyStep::Subject
        );
    }

    #[test]
    fn another_keys_signature_does_not_verify() {
        let (_, env) = signed();
        let stranger = Signer::from_seed(&[8; 32]);
        let mut keys = stranger.key_set();
        let mut lying = stranger.public_key();
        lying.keyid = Signer::from_seed(&[7; 32]).keyid().clone();
        keys.keys.push(lying);
        assert_eq!(
            verify(&env.bytes, &keys, Expect::default())
                .unwrap_err()
                .step,
            VerifyStep::Signature
        );
    }

    #[test]
    fn a_statement_of_another_type_is_refused_even_when_signed() {
        let signer = Signer::from_seed(&[7; 32]);
        let mut statement = Statement::new(&commit(), predicate());
        statement.predicate_type = "https://slsa.dev/provenance/v1".into();
        let env = signer.sign(&statement).unwrap();
        assert_eq!(
            verify(&env.bytes, &signer.key_set(), Expect::default())
                .unwrap_err()
                .step,
            VerifyStep::Statement
        );
    }

    #[test]
    fn a_pkcs8_pem_key_loads() {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let key = SigningKey::from_bytes(&[7; 32]);
        let pem = key
            .to_pkcs8_pem(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)
            .unwrap();
        let signer = Signer::from_pkcs8_pem(&pem).unwrap();
        assert_eq!(signer.keyid(), Signer::from_seed(&[7; 32]).keyid());
        assert!(matches!(
            Signer::from_pkcs8_pem("-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----"),
            Err(ProvenanceError::SigningKey(_))
        ));
    }

    #[test]
    fn commit_links_name_the_pushed_commit() {
        let parse = |url: &str| PushedCommit::from_link(&ExternalLink::parse(url).unwrap());
        assert_eq!(
            parse(&format!(
                "https://github.com/acme/widget/commit/{}",
                SHA.to_uppercase()
            )),
            Some(commit())
        );
        assert_eq!(
            parse(&format!("https://gitlab.com/group/sub/proj/-/commit/{SHA}")),
            Some(PushedCommit {
                repository: "https://gitlab.com/group/sub/proj".into(),
                sha: CommitSha::parse(SHA).unwrap(),
            })
        );
        assert_eq!(parse("https://github.com/acme/widget/commit/0123abc"), None);
        assert_eq!(parse("https://github.com/acme/widget/pull/7"), None);
        assert_eq!(
            parse(&format!("https://example.com/acme/commit/{SHA}")),
            None
        );
    }

    #[test]
    fn from_links_keeps_each_commit_once() {
        let links: Vec<ExternalLink> = [
            format!("https://github.com/acme/widget/commit/{SHA}"),
            "https://github.com/acme/widget/pull/7".to_string(),
            format!("https://github.com/acme/widget/commit/{SHA}"),
        ]
        .iter()
        .map(|u| ExternalLink::parse(u).unwrap())
        .collect();
        assert_eq!(PushedCommit::from_links(&links), vec![commit()]);
    }

    #[test]
    fn identifiers_refuse_what_they_are_not() {
        assert!(CommitSha::parse("abc").is_err());
        assert!(CommitSha::parse(&"g".repeat(40)).is_err());
        assert!(CommitSha::parse(&"a".repeat(64)).is_ok());
        assert!(EnvelopeDigest::parse(&"a".repeat(64)).is_err());
        assert!(EnvelopeDigest::parse(&format!("sha256:{}", "a".repeat(63))).is_err());
        assert!(KeyId::parse("xyz").is_err());
        assert!(serde_json::from_str::<CommitSha>("\"nope\"").is_err());
    }

    #[test]
    fn dispatch_provenance_refuses_unknown_fields() {
        assert!(serde_json::from_str::<DispatchProvenance>(r#"{"cluster":"a","x":1}"#).is_err());
        let full: DispatchProvenance =
            serde_json::from_str(r#"{"cluster":"a","provider":"p","sandbox_image":"i"}"#).unwrap();
        assert_eq!(full.cluster.as_deref(), Some("a"));
        assert_eq!(
            serde_json::to_string(&DispatchProvenance::default()).unwrap(),
            "{}"
        );
    }
}

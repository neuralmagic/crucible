//! What one launch resolved: which secrets a run gets, under which declared names, and where it
//! wants them.
//!
//! Nothing here holds bytes. An item is a secret id and a projection; the hub reads Vault at
//! dispatch and hands the values to [`crate::secrets::deliver`].

use crate::authz::model::Principal;
use crate::secrets::store::SecretRow;
use crate::secrets::{ProjectionKind, SecretName};

/// One resolved binding: which secret, under the name the pack declared, and where the run wants
/// it. The value is not here and never will be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantItem {
    pub secret_id: String,
    pub declared_name: SecretName,
    pub projection_kind: ProjectionKind,
    pub projection: String,
}

/// One resolved binding plus the owner and name the secret carries. The extra two belong to the
/// secret, which outlives the launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantMint {
    pub item: GrantItem,
    pub secret_name: SecretName,
    pub owner: Principal,
}

/// The mint a resolved binding becomes.
pub fn mint_from(binding: &crate::secrets::store::BindingRow, secret: &SecretRow) -> GrantMint {
    GrantMint {
        item: GrantItem {
            secret_id: secret.id.clone(),
            declared_name: binding.declared_name.clone(),
            projection_kind: binding.projection_kind,
            projection: binding.projection.clone(),
        },
        secret_name: secret.name.clone(),
        owner: secret.owner.clone(),
    }
}

//! `OwnerReference` builder generic over any `kube::Resource`. Used by both
//! reconcile loops so cascade-GC stays uniform across CR kinds.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::Resource;

use crate::error::{OperatorError, Result};

/// Build a controller `OwnerReference` for `cr`. `block_owner_deletion`
/// is set so the parent CR sticks around until cascade GC drains the
/// children.
pub(crate) fn owner_reference<R>(cr: &R) -> Result<OwnerReference>
where
    R: Resource<DynamicType = ()>,
{
    let meta = cr.meta();
    let uid = meta
        .uid
        .clone()
        .ok_or_else(|| OperatorError::MissingField("metadata.uid".into()))?;
    let name = meta
        .name
        .clone()
        .ok_or_else(|| OperatorError::MissingField("metadata.name".into()))?;
    Ok(OwnerReference {
        api_version: R::api_version(&()).into_owned(),
        kind: R::kind(&()).into_owned(),
        name,
        uid,
        controller: Some(true),
        block_owner_deletion: Some(true),
    })
}

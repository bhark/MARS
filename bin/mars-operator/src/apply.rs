//! Thin SSA wrappers around the kube API. Each function patches one resource
//! kind using the operator's shared field manager; `runtime_pdb` additionally
//! garbage-collects its sibling when the spec field is absent.

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim, Service};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{Api, Patch, PatchParams};
use serde_json::json;

use crate::children::labels;
use crate::crd::spec::MarsService;
use crate::error::Result;
use crate::reconcile::Ctx;

pub(crate) async fn configmap(ctx: &Ctx, ns: &str, cm: &ConfigMap) -> Result<()> {
    let api: Api<ConfigMap> = Api::namespaced(ctx.client.clone(), ns);
    let name = cm.metadata.name.as_deref().unwrap_or("");
    api.patch(
        name,
        &PatchParams::apply(crate::controller::FIELD_MANAGER).force(),
        &Patch::Apply(cm),
    )
    .await?;
    Ok(())
}

pub(crate) async fn pvc(ctx: &Ctx, ns: &str, pvc: &PersistentVolumeClaim) -> Result<()> {
    let api: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), ns);
    let name = pvc.metadata.name.as_deref().unwrap_or("");
    // create-only: PVC spec fields are largely immutable. if it exists we
    // leave it alone; mismatch surfaces via observed object events.
    if api.get_opt(name).await?.is_some() {
        return Ok(());
    }
    api.patch(
        name,
        &PatchParams::apply(crate::controller::FIELD_MANAGER).force(),
        &Patch::Apply(pvc),
    )
    .await?;
    Ok(())
}

pub(crate) async fn deployment(ctx: &Ctx, ns: &str, dep: &Deployment) -> Result<()> {
    let api: Api<Deployment> = Api::namespaced(ctx.client.clone(), ns);
    let name = dep.metadata.name.as_deref().unwrap_or("");
    api.patch(
        name,
        &PatchParams::apply(crate::controller::FIELD_MANAGER).force(),
        &Patch::Apply(dep),
    )
    .await?;
    Ok(())
}

pub(crate) async fn service(ctx: &Ctx, ns: &str, svc: &Service) -> Result<()> {
    let api: Api<Service> = Api::namespaced(ctx.client.clone(), ns);
    let name = svc.metadata.name.as_deref().unwrap_or("");
    api.patch(
        name,
        &PatchParams::apply(crate::controller::FIELD_MANAGER).force(),
        &Patch::Apply(svc),
    )
    .await?;
    Ok(())
}

/// Apply when present, garbage-collect when absent. Toggling the
/// CR's `runtime.podDisruptionBudget` off removes the sibling.
pub(crate) async fn runtime_pdb(ctx: &Ctx, ns: &str, svc_name: &str, pdb: Option<&PodDisruptionBudget>) -> Result<()> {
    let api: Api<PodDisruptionBudget> = Api::namespaced(ctx.client.clone(), ns);
    let name = labels::runtime_pdb_name(svc_name);
    match pdb {
        Some(obj) => {
            api.patch(
                &name,
                &PatchParams::apply(crate::controller::FIELD_MANAGER).force(),
                &Patch::Apply(obj),
            )
            .await?;
        }
        None => match api.delete(&name, &Default::default()).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => return Err(e.into()),
        },
    }
    Ok(())
}

pub(crate) async fn patch_status(
    ctx: &Ctx,
    name: &str,
    ns: &str,
    status_body: crate::crd::spec::MarsServiceStatus,
) -> Result<()> {
    let api: Api<MarsService> = Api::namespaced(ctx.client.clone(), ns);
    let body = json!({ "status": status_body });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&body))
        .await?;
    Ok(())
}

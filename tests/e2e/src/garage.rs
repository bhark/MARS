//! garage bootstrap via the admin api. assumes the garage Deployment is
//! applied; waits for the admin endpoint to respond, then assigns layout,
//! creates the bucket + access key, and writes an AK/SK Secret into the
//! namespace for MarsService.spec.{runtime,compiler}.envFrom.

use anyhow::{Context, Result, anyhow};
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{ObjectMeta, PostParams};
use kube::{Api, Client};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

const ADMIN_PORT: u16 = 3903;
const ADMIN_TOKEN: &str = "devdevdevdevdevdevdevdevdevdevdev"; // matches garage.yaml.tmpl
const BUCKET: &str = "mars-artifacts";
const KEY_NAME: &str = "mars-e2e-key";
const CRED_SECRET: &str = "mars-s3-credentials";

#[derive(Deserialize)]
struct NodeEntry {
    id: String,
}

#[derive(Deserialize)]
struct ClusterStatus {
    nodes: Vec<NodeEntry>,
}

#[derive(Deserialize)]
struct BucketInfo {
    id: String,
}

#[derive(Deserialize)]
struct KeyCreated {
    #[serde(rename = "accessKeyId")]
    access_key_id: String,
    #[serde(rename = "secretAccessKey")]
    secret_access_key: String,
}

pub async fn bootstrap(client: Arc<Client>, ns: &str) -> Result<()> {
    await_admin(&client, ns).await?;

    let status: ClusterStatus = admin_get(&client, ns, "/v2/GetClusterStatus").await?;
    let node_id = status
        .nodes
        .into_iter()
        .next()
        .map(|n| n.id)
        .ok_or_else(|| anyhow!("garage GetClusterStatus returned empty nodes"))?;
    info!(node = %node_id, "garage admin reachable");

    if !layout_ready(&client, ns).await? {
        let update = json!({
            "roles": [{
                "id": node_id,
                "zone": "dc1",
                "capacity": 1_073_741_824u64,
                "tags": [],
            }],
            "parameters": null,
        });
        let _ = admin_post(&client, ns, "/v2/UpdateClusterLayout", &update).await?;
        let apply = json!({ "version": 1 });
        let _ = admin_post(&client, ns, "/v2/ApplyClusterLayout", &apply).await?;
        info!("garage layout applied");
    }

    // create bucket; on 409 (already exists) fall back to GetBucketInfo to recover the id.
    let bucket_body = json!({ "globalAlias": BUCKET });
    let bucket_id = match admin_post_json::<BucketInfo>(&client, ns, "/v2/CreateBucket", &bucket_body).await {
        Ok(b) => b.id,
        Err(_) => {
            let info: BucketInfo = admin_get(&client, ns, &format!("/v2/GetBucketInfo?globalAlias={BUCKET}"))
                .await
                .context("garage GetBucketInfo fallback after CreateBucket failed")?;
            info.id
        }
    };

    let key_body = json!({ "name": KEY_NAME });
    let created: KeyCreated = admin_post_json(&client, ns, "/v2/CreateKey", &key_body)
        .await
        .context("garage create key")?;

    let allow = json!({
        "bucketId": bucket_id,
        "accessKeyId": created.access_key_id,
        "permissions": { "read": true, "write": true, "owner": true },
    });
    let _ = admin_post(&client, ns, "/v2/AllowBucketKey", &allow).await;

    write_credentials_secret(&client, ns, &created.access_key_id, &created.secret_access_key).await?;
    info!("garage bootstrap complete; credentials in Secret/{CRED_SECRET}");
    Ok(())
}

async fn await_admin(client: &Arc<Client>, ns: &str) -> Result<()> {
    crate::wait::until(
        "garage admin /v2/GetClusterStatus",
        Duration::from_secs(120),
        || async {
            match crate::http::get_with_bearer(
                client.clone(),
                ns,
                "garage",
                ADMIN_PORT,
                "/v2/GetClusterStatus",
                ADMIN_TOKEN,
            )
            .await
            {
                Ok(r) if r.status == 200 => Ok(Some(())),
                Ok(r) => {
                    tracing::debug!(status = r.status, "admin not ready");
                    Ok(None)
                }
                Err(_) => Ok(None),
            }
        },
    )
    .await
}

async fn layout_ready(client: &Arc<Client>, ns: &str) -> Result<bool> {
    // treat version > 0 as already applied.
    let resp = crate::http::get_with_bearer(
        client.clone(),
        ns,
        "garage",
        ADMIN_PORT,
        "/v2/GetClusterLayout",
        ADMIN_TOKEN,
    )
    .await?;
    if resp.status != 200 {
        return Ok(false);
    }
    let v: serde_json::Value = serde_json::from_slice(&resp.body).context("parse layout response")?;
    Ok(v.get("version").and_then(|x| x.as_u64()).unwrap_or(0) > 0)
}

async fn admin_get<T: for<'de> Deserialize<'de>>(client: &Arc<Client>, ns: &str, path: &str) -> Result<T> {
    let resp = crate::http::get_with_bearer(client.clone(), ns, "garage", ADMIN_PORT, path, ADMIN_TOKEN).await?;
    if resp.status != 200 {
        return Err(anyhow!("garage admin GET {path}: status {}", resp.status));
    }
    serde_json::from_slice(&resp.body).with_context(|| format!("parse {path} response"))
}

async fn admin_post(client: &Arc<Client>, ns: &str, path: &str, body: &serde_json::Value) -> Result<bytes::Bytes> {
    let resp = crate::http::post_json(client.clone(), ns, "garage", ADMIN_PORT, path, Some(ADMIN_TOKEN), body).await?;
    if resp.status >= 400 {
        return Err(anyhow!(
            "garage admin POST {path}: status {} body {}",
            resp.status,
            String::from_utf8_lossy(&resp.body),
        ));
    }
    Ok(resp.body)
}

async fn admin_post_json<T: for<'de> Deserialize<'de>>(
    client: &Arc<Client>,
    ns: &str,
    path: &str,
    body: &serde_json::Value,
) -> Result<T> {
    let bytes = admin_post(client, ns, path, body).await?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {path} response"))
}

async fn write_credentials_secret(client: &Client, ns: &str, ak: &str, sk: &str) -> Result<()> {
    let api: Api<Secret> = Api::namespaced(client.clone(), ns);
    let mut data = BTreeMap::new();
    data.insert("AWS_ACCESS_KEY_ID".to_string(), ByteString(ak.as_bytes().to_vec()));
    data.insert("AWS_SECRET_ACCESS_KEY".to_string(), ByteString(sk.as_bytes().to_vec()));
    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(CRED_SECRET.to_string()),
            ..Default::default()
        },
        type_: Some("Opaque".to_string()),
        data: Some(data),
        ..Default::default()
    };
    match api.create(&PostParams::default(), &secret).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(ae)) if ae.code == 409 => api
            .replace(CRED_SECRET, &PostParams::default(), &secret)
            .await
            .map(|_| ())
            .context("replace existing mars-s3-credentials Secret"),
        Err(e) => Err(e).context("create mars-s3-credentials Secret"),
    }
}

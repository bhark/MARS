//! one-shot HTTP via the kubernetes api-server proxy. avoids managing a
//! port-forward task lifetime; fine for low-volume requests (capabilities,
//! getmap, /metrics, garage admin api). use kubectl proxy or an in-cluster
//! Job for high volume.

use anyhow::{Context, Result, anyhow};
use http_body_util::BodyExt;
use kube::Client;
use kube::client::Body;
use std::sync::Arc;

pub struct Response {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub body: bytes::Bytes,
}

pub async fn get(client: Arc<Client>, ns: &str, service: &str, port: u16, path_and_query: &str) -> Result<Response> {
    request(client, ns, service, port, "GET", path_and_query, None, None).await
}

pub async fn post_json(
    client: Arc<Client>,
    ns: &str,
    service: &str,
    port: u16,
    path_and_query: &str,
    bearer: Option<&str>,
    body: &serde_json::Value,
) -> Result<Response> {
    let raw = serde_json::to_vec(body).context("serialize json body")?;
    request(
        client,
        ns,
        service,
        port,
        "POST",
        path_and_query,
        bearer,
        Some(("application/json", raw)),
    )
    .await
}

pub async fn get_with_bearer(
    client: Arc<Client>,
    ns: &str,
    service: &str,
    port: u16,
    path_and_query: &str,
    bearer: &str,
) -> Result<Response> {
    request(client, ns, service, port, "GET", path_and_query, Some(bearer), None).await
}

#[allow(clippy::too_many_arguments)]
async fn request(
    client: Arc<Client>,
    ns: &str,
    service: &str,
    port: u16,
    method: &str,
    path_and_query: &str,
    bearer: Option<&str>,
    body: Option<(&str, Vec<u8>)>,
) -> Result<Response> {
    let scheme = "http";
    let path = format!("/api/v1/namespaces/{ns}/services/{scheme}:{service}:{port}/proxy{path_and_query}");
    let mut builder = http::Request::builder().method(method).uri(&path);
    if let Some(token) = bearer {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let (body_bytes, builder) = match body {
        Some((ct, b)) => (b, builder.header("Content-Type", ct)),
        None => (Vec::new(), builder),
    };
    let req = builder.body(Body::from(body_bytes)).context("build http request")?;
    let resp = client
        .send(req)
        .await
        .with_context(|| format!("api-server proxy {method} {path}"))?;
    let (parts, body) = resp.into_parts();
    let bytes = body
        .collect()
        .await
        .map_err(|e| anyhow!("read response body: {e}"))?
        .to_bytes();
    Ok(Response {
        status: parts.status.as_u16(),
        headers: parts.headers,
        body: bytes,
    })
}

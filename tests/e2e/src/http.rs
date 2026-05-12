//! one-shot HTTP via the kubernetes api-server proxy. avoids managing a
//! port-forward task lifetime; fine for low-volume requests (capabilities,
//! getmap, /metrics). use kubectl proxy or an in-cluster Job for high volume.

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
    let scheme = "http";
    let path = format!("/api/v1/namespaces/{ns}/services/{scheme}:{service}:{port}/proxy{path_and_query}");
    let req = http::Request::builder()
        .method("GET")
        .uri(&path)
        .body(Body::from(Vec::<u8>::new()))
        .context("build http request")?;
    let resp = client
        .send(req)
        .await
        .with_context(|| format!("api-server proxy GET {path}"))?;
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

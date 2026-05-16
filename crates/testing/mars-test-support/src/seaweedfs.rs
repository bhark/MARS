//! seaweedfs testcontainer bring-up for s3-compatible integration tests.
//!
//! seaweedfs is the canonical "conditional_put = disabled" backend that MARS
//! singles out at `crates/adapters/mars-store-s3/src/manifest.rs:195`. it ships
//! a single-process `server -s3` mode that bundles master + volume + filer + s3
//! on one container, requires no IAM dance, and auto-creates buckets on first
//! write. minio is intentionally excluded (governance regression in 2025).
//!
//! the fixture exposes only primitive fields (no `S3Config`) so this crate
//! stays free of any adapter dependency, which the hexagonal-architecture
//! script enforces.

use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

/// fully booted seaweedfs s3 instance. host the container is kept alive via
/// the field so dropping the fixture stops the container.
pub struct SeaweedfsFixture {
    #[allow(dead_code)]
    container: ContainerAsync<GenericImage>,
    /// http base url for the s3 endpoint, e.g. `http://127.0.0.1:38231`.
    pub endpoint: String,
    /// dummy access key. seaweedfs' default s3 config accepts any signed
    /// request without validation, so this can be any non-empty string.
    pub access_key: String,
    /// dummy secret key. see `access_key`.
    pub secret_key: String,
    /// region to declare on the s3 client. seaweedfs ignores it; supplied so
    /// `AmazonS3Builder::with_region` is happy.
    pub region: String,
}

/// overrides for the seaweedfs container shape.
#[derive(Debug, Clone)]
pub struct SeaweedfsOptions {
    /// image tag for `chrislusf/seaweedfs`. defaults to a pinned recent stable.
    pub image_tag: String,
}

impl Default for SeaweedfsOptions {
    fn default() -> Self {
        // pinned to a recent stable. if seaweedfs ever changes the s3 ready
        // log line, bump this and update `WaitFor::message_on_stdout` below.
        Self {
            image_tag: "3.85".into(),
        }
    }
}

/// boot a seaweedfs s3 container with default options. dummy creds; bucket
/// auto-create on first write.
pub async fn boot_seaweedfs() -> SeaweedfsFixture {
    boot_seaweedfs_with(SeaweedfsOptions::default()).await
}

/// boot a seaweedfs s3 container with overridden options.
pub async fn boot_seaweedfs_with(opts: SeaweedfsOptions) -> SeaweedfsFixture {
    // `server -s3` brings up master + volume + filer + s3 in a single
    // process. the s3 endpoint listens on 8333.
    let container = GenericImage::new("chrislusf/seaweedfs", &opts.image_tag)
        .with_exposed_port(8333.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Start Seaweed S3 API Server"))
        .with_cmd(["server", "-s3"])
        .start()
        .await
        .expect("docker available; seaweedfs container starts");
    let port = container
        .get_host_port_ipv4(8333)
        .await
        .expect("seaweedfs exposes 8333");
    let endpoint = format!("http://127.0.0.1:{port}");
    SeaweedfsFixture {
        container,
        endpoint,
        access_key: "any".into(),
        secret_key: "any".into(),
        region: "us-east-1".into(),
    }
}

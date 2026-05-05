//! connection / topology configuration for the s3 adapter.

use mars_store::StoreError;
use object_store::aws::AmazonS3Builder;

/// S3 / S3-compatible connection settings.
///
/// `region` is required by AWS; for MinIO / R2 a placeholder like
/// `us-east-1` together with an explicit `endpoint` works. Credentials are
/// read from the standard AWS chain (env, profile, IMDS) when omitted.
#[derive(Debug, Clone, Default)]
pub struct S3Config {
    /// Optional custom endpoint (MinIO / R2 / localstack). None for AWS.
    pub endpoint: Option<String>,
    /// AWS region. Always required, even with a custom endpoint.
    pub region: String,
    /// Bucket name.
    pub bucket: String,
    /// Key prefix prepended to every object key. May be empty.
    pub prefix: String,
    /// Optional explicit access key (otherwise read from env/profile/imds).
    pub access_key_id: Option<String>,
    /// Optional explicit secret key.
    pub secret_access_key: Option<String>,
    /// Allow http endpoints (needed for minio in tests).
    pub allow_http: bool,
}

impl S3Config {
    /// Build an `AmazonS3Builder` from this config. The builder is returned
    /// unbuilt so callers can layer extra settings (retry config, etc.).
    pub fn builder(&self) -> AmazonS3Builder {
        let mut b = AmazonS3Builder::new()
            .with_bucket_name(&self.bucket)
            .with_region(&self.region)
            .with_allow_http(self.allow_http);
        if let Some(ep) = &self.endpoint {
            b = b.with_endpoint(ep);
        }
        if let (Some(ak), Some(sk)) = (&self.access_key_id, &self.secret_access_key) {
            b = b.with_access_key_id(ak).with_secret_access_key(sk);
        }
        b
    }

    /// Validate required fields without touching the network.
    pub fn validate(&self) -> Result<(), StoreError> {
        if self.bucket.is_empty() {
            return Err(StoreError::Backend("s3: bucket is required".into()));
        }
        if self.region.is_empty() {
            return Err(StoreError::Backend("s3: region is required".into()));
        }
        Ok(())
    }
}

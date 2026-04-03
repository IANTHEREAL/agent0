use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::timeout::TimeoutConfig;
use aws_sdk_s3::config::Builder as S3ConfigBuilder;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::presigning::{PresignedRequest, PresigningConfig};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{ChecksumAlgorithm, CompletedMultipartUpload, CompletedPart};
use bytes::Bytes;
use tokio::io::AsyncRead;

use crate::extensions::fs::backend::FsPresignedRequest;
use crate::extensions::fs::embedded::types::ObjectStoreBinding;

#[derive(Clone)]
pub(crate) struct FsS3Client {
    bucket: String,
    client: aws_sdk_s3::Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FsS3HeadObject {
    pub size: u64,
}

impl FsS3Client {
    pub(crate) async fn new(binding: &ObjectStoreBinding) -> Result<Self> {
        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        if let Some(region) = binding.region.as_deref() {
            loader = loader.region(Region::new(region.to_string()));
        }
        let shared = loader.load().await;

        let timeout_config = TimeoutConfig::builder()
            .operation_attempt_timeout(Duration::from_secs(600))
            .build();
        let retry_config = RetryConfig::standard().with_max_attempts(4);

        let mut builder = S3ConfigBuilder::from(&shared)
            .timeout_config(timeout_config)
            .retry_config(retry_config);
        if let Some(endpoint) = binding.endpoint.as_deref() {
            builder = builder.endpoint_url(endpoint);
        }
        if binding.force_path_style {
            builder = builder.force_path_style(true);
        }
        let s3_config = builder.build();

        Ok(Self {
            bucket: binding.bucket.clone(),
            client: aws_sdk_s3::Client::from_conf(s3_config),
        })
    }

    pub(crate) async fn get_object_bytes(&self, key: &str) -> Result<Bytes> {
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("fs9: GetObject failed for s3://{}/{}", self.bucket, key))?;
        let agg = out.body.collect().await.with_context(|| {
            format!(
                "fs9: failed to read GetObject body for s3://{}/{}",
                self.bucket, key
            )
        })?;
        Ok(agg.into_bytes())
    }

    pub(crate) async fn put_object(&self, key: &str, data: Bytes) -> Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(data))
            .send()
            .await?;
        Ok(())
    }

    pub(crate) async fn get_object_range_bytes(
        &self,
        key: &str,
        offset: u64,
        len: usize,
    ) -> Result<Bytes> {
        if len == 0 {
            return Ok(Bytes::new());
        }
        let end = offset
            .checked_add(u64::try_from(len).map_err(|_| anyhow!("range len exceeds u64"))?)
            .and_then(|v| v.checked_sub(1))
            .ok_or_else(|| anyhow!("range overflow"))?;
        let range = format!("bytes={offset}-{end}");

        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .range(range)
            .send()
            .await
            .with_context(|| {
                format!(
                    "fs9: ranged GetObject failed for s3://{}/{}",
                    self.bucket, key
                )
            })?;
        let agg = out.body.collect().await.with_context(|| {
            format!(
                "fs9: failed to read ranged GetObject body for s3://{}/{}",
                self.bucket, key
            )
        })?;
        Ok(agg.into_bytes())
    }

    pub(crate) async fn get_object_range_stream(
        &self,
        key: &str,
        offset: u64,
        len: usize,
    ) -> Result<Box<dyn AsyncRead + Unpin + Send + 'static>> {
        if len == 0 {
            return Ok(Box::new(tokio::io::empty()));
        }

        let end = offset
            .checked_add(u64::try_from(len).map_err(|_| anyhow!("range len exceeds u64"))?)
            .and_then(|v| v.checked_sub(1))
            .ok_or_else(|| anyhow!("range overflow"))?;
        let range = format!("bytes={offset}-{end}");

        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .range(range)
            .send()
            .await?;
        Ok(Box::new(out.body.into_async_read()))
    }

    pub(crate) async fn get_object_stream(
        &self,
        key: &str,
    ) -> Result<impl AsyncRead + Unpin + Send + 'static> {
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| {
                format!(
                    "fs9: streaming GetObject failed for s3://{}/{}",
                    self.bucket, key
                )
            })?;
        Ok(out.body.into_async_read())
    }

    pub(crate) async fn head_object(&self, key: &str) -> Result<FsS3HeadObject> {
        let out = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("fs9: HeadObject failed for s3://{}/{}", self.bucket, key))?;
        let size = out
            .content_length()
            .ok_or_else(|| anyhow!("fs9: missing content length from HeadObject"))?;
        let size = u64::try_from(size)
            .map_err(|_| anyhow!("fs9: negative content length from HeadObject"))?;
        Ok(FsS3HeadObject { size })
    }

    pub(crate) async fn delete_object(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| {
                format!("fs9: DeleteObject failed for s3://{}/{}", self.bucket, key)
            })?;
        Ok(())
    }

    pub(crate) async fn create_multipart_upload(
        &self,
        key: &str,
        checksum_algorithm: Option<&str>,
    ) -> Result<String> {
        let mut builder = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key);

        if matches!(checksum_algorithm, Some(alg) if alg.eq_ignore_ascii_case("crc32c")) {
            builder = builder.checksum_algorithm(ChecksumAlgorithm::Crc32C);
        }

        let out = builder.send().await.with_context(|| {
            format!(
                "fs9: CreateMultipartUpload failed for s3://{}/{}",
                self.bucket, key
            )
        })?;
        out.upload_id()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("fs9: missing upload_id from CreateMultipartUpload"))
    }

    pub(crate) async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: i32,
        data: Bytes,
    ) -> Result<String> {
        let out = self
            .client
            .upload_part()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(part_number)
            .body(ByteStream::from(data))
            .send()
            .await
            .with_context(|| {
                format!(
                    "fs9: UploadPart failed for s3://{}/{} upload_id={} part={}",
                    self.bucket, key, upload_id, part_number
                )
            })?;
        out.e_tag()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("fs9: missing etag from UploadPart"))
    }

    pub(crate) async fn complete_multipart_upload(
        &self,
        key: &str,
        upload_id: &str,
        parts: Vec<(i32, String, Option<String>)>,
    ) -> Result<()> {
        let mut completed = Vec::with_capacity(parts.len());
        for (part_number, etag, checksum_crc32c) in parts {
            let mut builder = CompletedPart::builder()
                .part_number(part_number)
                .e_tag(etag);
            if let Some(crc) = checksum_crc32c {
                builder = builder.checksum_crc32_c(crc);
            }
            completed.push(builder.build());
        }

        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(completed))
                    .build(),
            )
            .send()
            .await
            .with_context(|| {
                format!(
                    "fs9: CompleteMultipartUpload failed for s3://{}/{} upload_id={}",
                    self.bucket, key, upload_id
                )
            })?;
        Ok(())
    }

    pub(crate) async fn abort_multipart_upload(&self, key: &str, upload_id: &str) -> Result<()> {
        let res = self
            .client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(SdkError::ServiceError(service_err)) if service_err.err().is_no_such_upload() => {
                // Cleanup is idempotent; the upload is already gone.
                Ok(())
            }
            Err(err) => Err(anyhow!(err)).with_context(|| {
                format!(
                    "fs9: AbortMultipartUpload failed for s3://{}/{} upload_id={}",
                    self.bucket, key, upload_id
                )
            }),
        }
    }

    pub(crate) async fn presign_upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: i32,
        expires_in_secs: u64,
        checksum_crc32c: Option<&str>,
    ) -> Result<FsPresignedRequest> {
        let expires_in = Duration::from_secs(expires_in_secs);
        let expires_at = current_unix_timestamp()
            .checked_add(
                i64::try_from(expires_in_secs).map_err(|_| anyhow!("fs9: ttl exceeds i64"))?,
            )
            .ok_or_else(|| anyhow!("fs9: presign expiry overflow"))?;
        let config = PresigningConfig::expires_in(expires_in)?;
        let mut builder = self
            .client
            .upload_part()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(part_number);

        if let Some(crc) = checksum_crc32c {
            builder = builder.checksum_crc32_c(crc);
        }

        let request = builder.presigned(config).await.with_context(|| {
            format!(
                "fs9: presign UploadPart failed for s3://{}/{} upload_id={} part={}",
                self.bucket, key, upload_id, part_number
            )
        })?;
        Ok(presigned_request_to_fs(request, expires_at))
    }

    pub(crate) async fn presign_get_object(
        &self,
        key: &str,
        expires_in_secs: u64,
    ) -> Result<FsPresignedRequest> {
        let expires_in = Duration::from_secs(expires_in_secs);
        let expires_at = current_unix_timestamp()
            .checked_add(
                i64::try_from(expires_in_secs).map_err(|_| anyhow!("fs9: ttl exceeds i64"))?,
            )
            .ok_or_else(|| anyhow!("fs9: presign expiry overflow"))?;
        let config = PresigningConfig::expires_in(expires_in)?;
        let request = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(config)
            .await
            .with_context(|| {
                format!(
                    "fs9: presign GetObject failed for s3://{}/{}",
                    self.bucket, key
                )
            })?;
        Ok(presigned_request_to_fs(request, expires_at))
    }
}

fn presigned_request_to_fs(request: PresignedRequest, expires_at: i64) -> FsPresignedRequest {
    let headers = request
        .headers()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    FsPresignedRequest {
        method: request.method().to_string(),
        url: request.uri().to_string(),
        headers,
        expires_at,
    }
}

fn current_unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

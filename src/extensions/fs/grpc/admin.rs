//! Minimal FsPlaneAdmin client used to materialize JuiceFS volumes before
//! handing a tenant to the fs9 data plane.

use std::time::Duration;

use anyhow::{anyhow, Result};
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

use crate::auth::fs_plane_token::{mint_fs_plane_admin_token, Auth9MintConfig};
use crate::extensions::fs::grpc::errors::{fs_error_to_anyhow, status_to_anyhow};
use crate::extensions::fs::grpc::proto::fs_plane_admin_client::FsPlaneAdminClient;
use crate::extensions::fs::grpc::proto::{
    init_volume_response, InitVolumeRequest, InitVolumeResponse,
};

const INIT_VOLUME_TIMEOUT: Duration = Duration::from_secs(30);
const CACHE_SIZE_MB: u32 = 16;

/// Ensure the fs9 volume for `tenant_id` exists. The RPC is idempotent on fs9.
///
/// Deliberately no process-wide success cache here: db9-server must re-observe
/// lifecycle state before each backend construction so teardown cannot be
/// bypassed by an earlier successful InitVolume.
///
/// The db9-server lifecycle read is an early fail-closed guard. It is not
/// atomic with this remote call, so production fs9 must enforce the same
/// absent/ENABLED-only predicate inside `FsPlaneAdmin.InitVolume` immediately
/// before creating or mounting the volume.
pub(crate) async fn ensure_juicefs_volume(
    channel: Channel,
    mint_cfg: &Auth9MintConfig,
    tenant_id: &str,
) -> Result<()> {
    let volume_id = crate::extensions::fs::jfs_volume_id(tenant_id);
    init_volume(channel, mint_cfg, tenant_id, &volume_id).await
}

async fn init_volume(
    channel: Channel,
    mint_cfg: &Auth9MintConfig,
    tenant_id: &str,
    volume_id: &str,
) -> Result<()> {
    let token = mint_fs_plane_admin_token(mint_cfg, tenant_id).await?;
    let bearer: MetadataValue<_> = format!("Bearer {}", token.token.as_ref())
        .parse()
        .map_err(|e| anyhow!("fs9 admin: build bearer metadata: {e}"))?;

    let mut req = tonic::Request::new(InitVolumeRequest {
        volume_id: volume_id.to_string(),
        meta_url: juicefs_meta_url(volume_id),
        cache_size_mb: CACHE_SIZE_MB,
        server_writeback: false,
    });
    req.metadata_mut().insert("authorization", bearer);
    req.set_timeout(INIT_VOLUME_TIMEOUT);

    let resp: InitVolumeResponse = fs_plane_admin_client(channel)
        .init_volume(req)
        .await
        .map_err(|status| status_to_anyhow(status, &format!("init_volume {volume_id}")))?
        .into_inner();

    match resp.result {
        Some(init_volume_response::Result::Success(success)) => {
            if success.created {
                crate::metrics::record_fs9_juicefs_lifecycle(tenant_id, "created");
                tracing::info!(tenant_id, volume_id, "fs9 volume initialized");
            } else {
                crate::metrics::record_fs9_juicefs_lifecycle(tenant_id, "existing");
                tracing::debug!(tenant_id, volume_id, "fs9 volume already initialized");
            }
            Ok(())
        }
        Some(init_volume_response::Result::Error(err)) => {
            crate::metrics::record_fs9_juicefs_lifecycle(tenant_id, "err");
            Err(fs_error_to_anyhow(err, &format!("init_volume {volume_id}")))
        }
        None => {
            crate::metrics::record_fs9_juicefs_lifecycle(tenant_id, "err");
            Err(anyhow!("fs9 InitVolume response missing result oneof"))
        }
    }
}

fn fs_plane_admin_client(channel: Channel) -> FsPlaneAdminClient<Channel> {
    const MESSAGE_SIZE_CEILING: usize = 16 * 1024 * 1024;
    FsPlaneAdminClient::new(channel)
        .max_decoding_message_size(MESSAGE_SIZE_CEILING)
        .max_encoding_message_size(MESSAGE_SIZE_CEILING)
}

fn juicefs_meta_url(volume_id: &str) -> String {
    let pd = crate::extensions::fs::juicefs_pd_endpoints_raw();
    juicefs_meta_url_with_pd(&pd, volume_id)
}

fn juicefs_meta_url_with_pd(pd_endpoints: &str, volume_id: &str) -> String {
    format!("tikv://{pd_endpoints}?keyspace={volume_id}&gc-interval=0")
}

#[cfg(test)]
mod tests {
    #[test]
    fn juicefs_meta_url_includes_matching_keyspace() {
        assert_eq!(
            super::juicefs_meta_url_with_pd("pd1:2379,pd2:2379", "jfs_t_tenant_abc"),
            "tikv://pd1:2379,pd2:2379?keyspace=jfs_t_tenant_abc&gc-interval=0"
        );
    }

    #[test]
    fn juicefs_meta_url_is_canonical_for_whitespace_input() {
        // Operator-provided `PD_ENDPOINTS` with spaces/empty segments must not
        // leak into the InitVolume meta URL: canonicalization yields the same
        // verbatim-interpolated endpoint set the lifecycle guard parses.
        let pd = crate::extensions::fs::canonicalize_juicefs_pd_endpoints("pd1:2379, pd2:2379, ,");
        assert_eq!(
            super::juicefs_meta_url_with_pd(&pd, "jfs_t_tenant_abc"),
            "tikv://pd1:2379,pd2:2379?keyspace=jfs_t_tenant_abc&gc-interval=0"
        );
    }
}

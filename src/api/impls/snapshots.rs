use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum_extra::extract::CookieJar;
use headers::Host;
use http::Method;

use agentenv_http_server::apis::snapshots::*;
use agentenv_http_server::models;

use crate::snapshot::{squash_snapshot, SnapshotAlias, SnapshotId, SnapshotRecord, SnapshotSource};

use super::pagination::PaginationCursor;
use super::ApiImpl;

impl From<SnapshotRecord> for models::SnapshotInfo {
    fn from(record: SnapshotRecord) -> Self {
        let snapshot_id = record.id.to_string();
        let image_ref = record.published_rootfs_image_ref().map(str::to_owned);
        let names = if let Some(alias) = record.alias {
            vec![alias.to_string()]
        } else {
            vec![]
        };
        let (rootfs_layer_count, memory_layer_count, chain_size_mb) = record
            .committed
            .as_ref()
            .map(committed_chain_stats)
            .unwrap_or((None, None, None));
        models::SnapshotInfo {
            snapshot_id,
            names,
            cpu_count: record.resources.cpu_count,
            memory_mb: record.resources.memory_mib,
            disk_size_mb: record.resources.disk_size_mib,
            created_at: chrono::DateTime::<chrono::Utc>::from(system_time_from_unix_ms(
                record.created_at_unix_ms,
            )),
            updated_at: chrono::DateTime::<chrono::Utc>::from(system_time_from_unix_ms(
                record.updated_at_unix_ms,
            )),
            image_ref,
            rootfs_layer_count,
            memory_layer_count,
            chain_size_mb,
        }
    }
}

/// Chain observability facts for a committed snapshot: rootfs layer count,
/// memory layer count, and the total committed layer bytes (rootfs + memory
/// + attached drives) in MiB, before cross-snapshot dedup.
fn committed_chain_stats(
    committed: &crate::snapshot::CommittedSnapshot,
) -> (Option<i32>, Option<i32>, Option<i64>) {
    fn layer_ref_size(layer: &crate::snapshot::OverlaybdLayerRef) -> u64 {
        match layer {
            crate::snapshot::OverlaybdLayerRef::Managed(managed) => managed.size,
            crate::snapshot::OverlaybdLayerRef::External(external) => external.size,
        }
    }

    let mut total_bytes: u64 = committed.rootfs_layers.iter().map(layer_ref_size).sum();
    total_bytes += committed
        .memory_layers
        .iter()
        .map(|layer| layer.size)
        .sum::<u64>();
    for drive in &committed.attached_drives {
        let crate::snapshot::CommittedAttachedDrive::Overlaybd { layers, .. } = drive;
        total_bytes += layers.iter().map(layer_ref_size).sum::<u64>();
    }

    (
        i32::try_from(committed.rootfs_layers.len()).ok(),
        i32::try_from(committed.memory_layers.len()).ok(),
        i64::try_from(total_bytes / (1024 * 1024)).ok(),
    )
}

fn system_time_from_unix_ms(unix_ms: i64) -> SystemTime {
    if unix_ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(unix_ms as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis(unix_ms.unsigned_abs())
    }
}

#[async_trait]
impl Snapshots<()> for ApiImpl {
    type Claims = super::Claims;

    async fn snapshots_get(
        &self,
        _method: &Method,
        _host: &Host,
        _cookies: &CookieJar,
        _claims: &Self::Claims,
        query_params: &models::SnapshotsGetQueryParams,
    ) -> Result<SnapshotsGetResponse, ()> {
        let cursor = match query_params.next_token.as_deref() {
            Some(token) => match PaginationCursor::<SnapshotId>::parse(token) {
                Ok(cursor) => cursor,
                Err(err) => {
                    return Ok(SnapshotsGetResponse::Status400_BadRequest(Self::error(
                        400,
                        format!("invalid next token: {}", err),
                    )));
                }
            },
            None => PaginationCursor::new(SystemTime::now(), SnapshotId::max()),
        };

        let summaries = match self
            .snapshot_manager
            .list(crate::snapshot::SnapshotListFilter::sandbox_snapshots(
                query_params.sandbox_id.clone(),
                query_params.name.clone(),
            ))
            .await
        {
            Ok(summaries) => summaries,
            Err(err) => {
                return Ok(SnapshotsGetResponse::Status500_ServerError(
                    Self::snapshot_manager_error(&err),
                ));
            }
        };

        let page = cursor.paginate_sorted(
            summaries,
            query_params.limit,
            |record, cursor| {
                PaginationCursor::compare_desc(
                    system_time_from_unix_ms(record.created_at_unix_ms),
                    &record.id,
                    cursor.time(),
                    cursor.value(),
                )
            },
            |record| {
                PaginationCursor::new(
                    system_time_from_unix_ms(record.created_at_unix_ms),
                    record.id.clone(),
                )
            },
        );

        Ok(
            SnapshotsGetResponse::Status200_SuccessfullyReturnedSnapshots {
                body: page
                    .items
                    .into_iter()
                    .map(models::SnapshotInfo::from)
                    .collect(),
                x_next_token: page.next_token,
            },
        )
    }

    async fn snapshots_snapshot_id_get(
        &self,
        _method: &Method,
        _host: &Host,
        _cookies: &CookieJar,
        _claims: &Self::Claims,
        path_params: &models::SnapshotsSnapshotIdGetPathParams,
    ) -> Result<SnapshotsSnapshotIdGetResponse, ()> {
        match self.snapshot_manager.get(&path_params.snapshot_id).await {
            // Scope this endpoint to sandbox-sourced snapshots so it stays
            // consistent with the list API, which only exposes
            // `SnapshotSourceKind::Sandbox` records. Template records are
            // surfaced through the template APIs instead.
            Ok(Some(record)) if matches!(record.source, SnapshotSource::Sandbox { .. }) => Ok(
                SnapshotsSnapshotIdGetResponse::Status200_SuccessfullyReturnedTheSnapshot(
                    models::SnapshotInfo::from(record),
                ),
            ),
            Ok(_) => Ok(SnapshotsSnapshotIdGetResponse::Status404_NotFound(
                Self::error(
                    404,
                    format!("snapshot '{}' not found", path_params.snapshot_id),
                ),
            )),
            Err(err) => Ok(SnapshotsSnapshotIdGetResponse::Status500_ServerError(
                Self::snapshot_manager_error(&err),
            )),
        }
    }

    async fn snapshots_snapshot_id_delete(
        &self,
        _method: &Method,
        _host: &Host,
        _cookies: &CookieJar,
        _claims: &Self::Claims,
        path_params: &models::SnapshotsSnapshotIdDeletePathParams,
    ) -> Result<SnapshotsSnapshotIdDeleteResponse, ()> {
        // Same visibility scope as GET and the list: this API only knows
        // sandbox-sourced snapshots. A template reached through this endpoint
        // is refused rather than hidden, because "204 but it still exists"
        // would be a lie and silently deleting a template would remove an
        // artifact shared by every sandbox launched from it.
        match self.snapshot_manager.get(&path_params.snapshot_id).await {
            Ok(Some(record)) if !matches!(record.source, SnapshotSource::Sandbox { .. }) => {
                return Ok(SnapshotsSnapshotIdDeleteResponse::Status409_TheIdNamesATemplate(
                    Self::error(
                        409,
                        format!(
                            "'{}' is a template, not a sandbox snapshot; manage it through the template API",
                            path_params.snapshot_id
                        ),
                    ),
                ));
            }
            // Absent is success: delete is idempotent so sweepers can retry.
            Ok(_) => {}
            Err(err) => {
                return Ok(SnapshotsSnapshotIdDeleteResponse::Status500_ServerError(
                    Self::snapshot_manager_error(&err),
                ));
            }
        }
        match self.snapshot_manager.delete(&path_params.snapshot_id).await {
            Ok(()) => Ok(SnapshotsSnapshotIdDeleteResponse::Status204_SnapshotDeleted),
            Err(err) => Ok(SnapshotsSnapshotIdDeleteResponse::Status500_ServerError(
                Self::snapshot_manager_error(&err),
            )),
        }
    }

    async fn snapshots_snapshot_id_squash_post(
        &self,
        _method: &Method,
        _host: &Host,
        _cookies: &CookieJar,
        _claims: &Self::Claims,
        path_params: &models::SnapshotsSnapshotIdSquashPostPathParams,
        body: &models::SnapshotSquashRequest,
    ) -> Result<SnapshotsSnapshotIdSquashPostResponse, ()> {
        let alias = match &body.name {
            Some(name) => match SnapshotAlias::parse(name) {
                Ok(alias) => Some(alias),
                Err(err) => {
                    return Ok(SnapshotsSnapshotIdSquashPostResponse::Status400_BadRequest(
                        Self::error(400, format!("invalid snapshot alias: {err}")),
                    ));
                }
            },
            None => None,
        };

        // The squash itself is repository-side work: it merges committed
        // layers and publishes a new snapshot, so no sandbox is involved and
        // no VM is paused.
        match squash_snapshot(&self.snapshot_manager, &path_params.snapshot_id, alias).await {
            Ok(outcome) => Ok(
                SnapshotsSnapshotIdSquashPostResponse::Status201_SquashedSnapshotPublished(
                    models::SnapshotInfo::from(outcome.record()),
                ),
            ),
            Err(err) => {
                let message = format!("{err:#}");
                if message.contains("not found") {
                    return Ok(SnapshotsSnapshotIdSquashPostResponse::Status404_NotFound(
                        Self::error(
                            404,
                            format!("snapshot '{}' not found", path_params.snapshot_id),
                        ),
                    ));
                }
                // Unsupported chain shapes (remote layers, snapshots that are
                // not ready) are caller-visible input problems, not faults.
                if message.contains("cannot squash") || message.contains("is not ready") {
                    return Ok(SnapshotsSnapshotIdSquashPostResponse::Status400_BadRequest(
                        Self::error(400, message),
                    ));
                }
                tracing::warn!(
                    snapshot_ref = %path_params.snapshot_id,
                    error = %message,
                    "failed to squash snapshot"
                );
                Ok(
                    SnapshotsSnapshotIdSquashPostResponse::Status500_ServerError(Self::error(
                        500, message,
                    )),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{
        rootfs_snapshot_image_tag, CommittedSnapshot, PersistedDiskImagePublication,
    };

    #[test]
    fn snapshot_info_includes_published_rootfs_image_ref() {
        let mut record = SnapshotRecord::mock_ready(CommittedSnapshot::mock());
        let tag = rootfs_snapshot_image_tag(&record.id);
        let expected = format!("registry.example/ns/app:{tag}");
        record.committed.as_mut().unwrap().disk_publications =
            vec![PersistedDiskImagePublication {
                image_ref: expected.clone(),
                tag,
                manifest_digest: "sha256:manifest".to_string(),
                repo_blob_url: "https://registry.example/v2/ns/app/blobs".to_string(),
            }];

        let info = models::SnapshotInfo::from(record);

        assert_eq!(info.image_ref.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn snapshot_info_omits_image_ref_without_publication() {
        let record = SnapshotRecord::mock_ready(CommittedSnapshot::mock());

        let info = models::SnapshotInfo::from(record);

        assert_eq!(info.image_ref, None);
        let serialized = serde_json::to_value(&info).expect("serialize SnapshotInfo");
        assert!(serialized.get("imageRef").is_none());
    }
}

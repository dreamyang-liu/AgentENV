//! Repository-side snapshot squashing.
//!
//! A capture can only compact the layers the capturing sandbox produced
//! itself: layers inherited from the snapshot it launched from are immutable
//! shared repository state. So every re-board adds one permanent layer to the
//! prefix, and a sandbox branched from a deep snapshot inherits that whole
//! prefix — shortening its compaction cycles and, across generations, walking
//! toward the LSMT format's 255-layer stack limit.
//!
//! Squashing breaks that ratchet from the repository side: it merges a
//! committed snapshot's whole chain (prefix included) into a single layer and
//! publishes the result as an equivalent *new* snapshot. The source snapshot
//! is never modified, so existing snapshots and running sandboxes keep their
//! layers; callers branch from the squashed twin instead, and its children
//! start from a one-layer prefix.
//!
//! Cost model: merge throughput is bound by the snapshot store's sequential
//! write bandwidth (chain bytes / write bandwidth). The merged layer is
//! additive storage until the source snapshot's layers become unreferenced
//! and are reclaimed, because the original layers stay shared with the
//! snapshots that still reference them.
//!
//! Not squashed in this version: attached-drive chains keep their own layer
//! stacks (they are separate block devices with separate limits).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use overlaybd::config::{load_image_config, ImageConfig, LayerConfig};
use tracing::{debug, info};
use uuid::Uuid;

use crate::cfg::ConfigManager;
use crate::image::local_layer::SELF_CONTAINED_BASE_LAYER_FILE;
use crate::sandbox::{FirecrackerSnapshotManifest, OverlaybdCompactOutput};
use crate::snapshot::{
    SnapshotAlias, SnapshotId, SnapshotManager, SnapshotPublishMetadata, SnapshotPublishSource,
    SnapshotRecord, SnapshotSource,
};

/// Removes a staging directory when dropped so a failed squash leaves no
/// partially merged layers behind.
struct StagingDirGuard(PathBuf);

impl Drop for StagingDirGuard {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    dir = %self.0.display(),
                    error = %error,
                    "failed to clean up snapshot squash staging dir"
                );
            }
        }
    }
}

/// Outcome of a squash request.
#[derive(Debug)]
pub enum SquashOutcome {
    /// A squashed twin was published.
    Squashed(SnapshotRecord),
    /// The source chain was already a single layer; nothing to merge.
    AlreadyFlat(SnapshotRecord),
}

impl SquashOutcome {
    pub fn record(self) -> SnapshotRecord {
        match self {
            Self::Squashed(record) | Self::AlreadyFlat(record) => record,
        }
    }
}

/// Merges a committed snapshot's rootfs (and memory) chain into single layers
/// and publishes the result as a new snapshot.
///
/// Returns [`SquashOutcome::AlreadyFlat`] with the source record when the
/// chain needs no merging, so callers can treat squashing as idempotent.
#[tracing::instrument(skip(manager, id_or_alias), fields(snapshot_ref = %id_or_alias.as_ref()))]
pub async fn squash_snapshot(
    manager: &SnapshotManager,
    id_or_alias: impl AsRef<str>,
    alias: Option<SnapshotAlias>,
) -> Result<SquashOutcome> {
    let id_or_alias = id_or_alias.as_ref();
    let record = manager
        .get(id_or_alias)
        .await?
        .with_context(|| format!("snapshot '{id_or_alias}' not found"))?;
    let committed = record
        .committed
        .as_ref()
        .with_context(|| format!("snapshot '{id_or_alias}' is not ready"))?
        .clone();

    // Resolving materializes node-local image configs and vm_state; the
    // returned handle holds the cache lease those paths depend on, so it must
    // outlive publication below.
    let runnable = manager.resolve_runnable(record.clone()).await?;
    let manifest = runnable.manifest();

    let rootfs_lowers = local_lowers(&manifest.rootfs.image_config_path, "rootfs")?;
    let memory_lowers = match manifest.memory.as_ref() {
        Some(memory) => local_lowers(&memory.image_config_path, "memory")?,
        None => Vec::new(),
    };
    if rootfs_lowers.len() <= 1 && memory_lowers.len() <= 1 {
        debug!(
            rootfs_layers = rootfs_lowers.len(),
            memory_layers = memory_lowers.len(),
            "snapshot chain is already flat; skipping squash"
        );
        return Ok(SquashOutcome::AlreadyFlat(record));
    }

    let staging_root = ConfigManager::global_config()
        .home_path
        .join("squash-staging")
        .join(Uuid::now_v7().to_string());
    tokio::fs::create_dir_all(&staging_root)
        .await
        .with_context(|| format!("create squash staging dir {}", staging_root.display()))?;
    let _staging_guard = StagingDirGuard(staging_root.clone());

    info!(
        rootfs_layers = rootfs_lowers.len(),
        memory_layers = memory_lowers.len(),
        "squashing snapshot chain"
    );

    // Rootfs layers must stay raw; only memory layers may be compressed.
    // A chain that needs no merging keeps its source config verbatim, which
    // also preserves resolver-written details such as download overrides.
    let rootfs_config_path = merge_chain_into_image_config(
        &rootfs_lowers,
        &staging_root.join("rootfs"),
        "rootfs",
        OverlaybdCompactOutput::Raw,
    )
    .await?
    .unwrap_or_else(|| manifest.rootfs.image_config_path.clone());
    let memory_config_path = match manifest.memory.as_ref() {
        Some(memory) => Some(
            merge_chain_into_image_config(
                &memory_lowers,
                &staging_root.join("memory"),
                "memory",
                OverlaybdCompactOutput::from_memory_snapshot_config(
                    &ConfigManager::global_config().memory_snapshot,
                ),
            )
            .await?
            .unwrap_or_else(|| memory.image_config_path.clone()),
        ),
        None => None,
    };

    let attached_drives = manifest.extra_drives();
    let squashed_manifest = match (manifest.vm_state.as_ref(), manifest.memory.as_ref()) {
        (Some(vm_state), Some(memory)) => FirecrackerSnapshotManifest::new(
            vm_state.path.clone(),
            memory_config_path.clone().expect("memory config was built"),
            memory.virtual_size,
            rootfs_config_path.clone(),
            manifest.rootfs.virtual_size,
            &attached_drives,
        ),
        _ => FirecrackerSnapshotManifest::new_disk_only(
            rootfs_config_path.clone(),
            manifest.rootfs.virtual_size,
            &attached_drives,
        ),
    }
    .context("build squashed snapshot manifest")?;

    let metadata = SnapshotPublishMetadata {
        id: SnapshotId::generate(),
        alias,
        source: match &record.source {
            SnapshotSource::Sandbox { source_sandbox_id } => SnapshotPublishSource::Sandbox {
                source_sandbox_id: source_sandbox_id.clone(),
            },
            SnapshotSource::Template { .. } => SnapshotPublishSource::Template,
        },
        context: committed.context.clone(),
        startup: committed.startup.clone(),
        resources: record.resources,
        runtime_versions: committed.runtime_versions.clone(),
        virtualization_mode: committed.virtualization_mode,
        image_configs: committed.image_configs.clone(),
        custom_extension_params: committed.custom_extension_params.clone(),
    };
    let published = manager
        .publish(metadata, squashed_manifest)
        .await
        .context("publish squashed snapshot")?;
    // Keep the source's runtime lease alive until publication has imported
    // every artifact it referenced.
    drop(runnable);

    info!(squashed_snapshot_id = %published.id, "published squashed snapshot");
    Ok(SquashOutcome::Squashed(published))
}

/// Reads an image config and returns its lowers, requiring every layer to be
/// present as a local file.
///
/// Remote lowers (object-storage or registry backed, materialized lazily)
/// carry no local path and cannot be merged in place; squashing those would
/// mean downloading the whole chain first, which this version does not do.
fn local_lowers(image_config_path: &Path, label: &str) -> Result<Vec<LayerConfig>> {
    let config = load_image_config(image_config_path)
        .with_context(|| format!("load {label} image config {}", image_config_path.display()))?;
    if let Some(index) = config
        .lowers
        .iter()
        .position(|lower| lower.file.trim().is_empty())
    {
        bail!(
            "cannot squash {label} chain: layer {index} has no local file (remote layers are unsupported)"
        );
    }
    Ok(config.lowers)
}

/// Merges `lowers` into one layer under `output_dir` and writes an image
/// config referencing it.
///
/// Returns `None` when the chain holds fewer than two layers: there is
/// nothing to merge, and the caller should keep using the source config
/// (rewriting it would drop resolver-written fields, and a zero-layer config
/// is not even loadable).
async fn merge_chain_into_image_config(
    lowers: &[LayerConfig],
    output_dir: &Path,
    label: &str,
    output_mode: OverlaybdCompactOutput,
) -> Result<Option<PathBuf>> {
    if lowers.len() < 2 {
        return Ok(None);
    }
    tokio::fs::create_dir_all(output_dir)
        .await
        .with_context(|| format!("create {label} squash dir {}", output_dir.display()))?;

    // Name the merged layer with the repository's self-contained-base
    // convention: publication accepts descriptorless local layers only under
    // the recognized runtime-generated names, and a squashed chain is exactly
    // a self-contained base layer.
    let output_path = output_dir.join(SELF_CONTAINED_BASE_LAYER_FILE);
    let merged = crate::sandbox::compact_layers(lowers, &output_path, output_mode)
        .await
        .with_context(|| format!("merge {label} chain into one layer"))?
        .with_context(|| format!("{label} chain merge produced no layer"))?;

    let image_config = ImageConfig {
        lowers: vec![LayerConfig {
            file: merged.display().to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let config_path = output_dir.join("image.json");
    let bytes = serde_json::to_vec_pretty(&image_config)
        .with_context(|| format!("serialize squashed {label} image config"))?;
    tokio::fs::write(&config_path, bytes)
        .await
        .with_context(|| format!("write squashed {label} image config"))?;
    Ok(Some(config_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(path: &Path, lowers: &[&str]) {
        let lowers: Vec<_> = lowers
            .iter()
            .map(|file| serde_json::json!({ "file": file }))
            .collect();
        std::fs::create_dir_all(path.parent().unwrap()).expect("config dir");
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "repoBlobUrl": "",
                "lowers": lowers,
                "upper": {},
                "resultFile": ""
            }))
            .expect("serialize"),
        )
        .expect("write config");
    }

    #[test]
    fn local_lowers_rejects_remote_layers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("image.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "repoBlobUrl": "s3://bucket/prefix/managed-layers",
                "lowers": [
                    { "file": "/local/a.commit" },
                    { "digest": "sha256:remote", "size": 4096 }
                ],
                "upper": {},
                "resultFile": ""
            }))
            .expect("serialize"),
        )
        .expect("write config");

        let err = local_lowers(&path, "rootfs").expect_err("remote layers must be rejected");
        let message = format!("{err:#}");
        assert!(message.contains("cannot squash rootfs chain"), "{message}");
        assert!(message.contains("layer 1"), "{message}");
    }

    #[test]
    fn local_lowers_returns_all_local_layers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("image.json");
        write_config(&path, &["/local/a.commit", "/local/b.commit"]);

        let lowers = local_lowers(&path, "rootfs").expect("local chain should parse");
        assert_eq!(lowers.len(), 2);
        assert_eq!(lowers[1].file, "/local/b.commit");
    }

    #[tokio::test]
    async fn chains_shorter_than_two_layers_are_left_alone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let only = LayerConfig {
            file: "/local/only.commit".to_string(),
            digest: "sha256:only".to_string(),
            size: 42,
            ..Default::default()
        };

        // Single layer: nothing to merge, caller keeps the source config.
        assert!(merge_chain_into_image_config(
            std::slice::from_ref(&only),
            &temp.path().join("rootfs"),
            "rootfs",
            OverlaybdCompactOutput::Raw,
        )
        .await
        .expect("single-layer chain is allowed")
        .is_none());

        // Empty memory chain (a resolved snapshot may legitimately have none):
        // must not synthesize a zero-layer config, which cannot be loaded.
        assert!(merge_chain_into_image_config(
            &[],
            &temp.path().join("memory"),
            "memory",
            OverlaybdCompactOutput::Raw,
        )
        .await
        .expect("empty chain is allowed")
        .is_none());

        assert!(!temp.path().join("rootfs").exists());
        assert!(!temp.path().join("memory").exists());
    }
}

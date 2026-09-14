use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::sandbox::ExtraDrive;

pub(crate) const MANIFEST_FORMAT_VERSION: u32 = 1;

pub(crate) fn combine_disk_delta_empty(
    values: impl IntoIterator<Item = Option<bool>>,
) -> Option<bool> {
    let mut unknown = false;
    for value in values {
        match value {
            Some(false) => return Some(false),
            None => unknown = true,
            Some(true) => {}
        }
    }
    if unknown {
        None
    } else {
        Some(true)
    }
}

/// Manifest describing the on-disk layout of a Firecracker snapshot.
///
/// This is intentionally decoupled from in-memory snapshot representations.
/// Snapshot-layer retrieve artifacts based on the manifest during snapshot
/// publication, and reconstruct the manifest with hydrated paths during snapshot resolution.
///
/// All paths in the manifest should be absolute.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirecrackerSnapshotManifest {
    /// Schema/version marker for persisted manifest format.
    pub version: u32,
    /// Capture-relative disk delta only, before compaction; memory is excluded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_empty: Option<bool>,
    /// Firecracker VM state artifact. `None` for disk-only snapshots, which
    /// carry no resumable VM state and can only boot fresh from their rootfs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm_state: Option<FirecrackerVmStateArtifacts>,
    /// Memory snapshot artifacts. `None` for disk-only snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<FirecrackerMemoryArtifacts>,
    pub rootfs: FirecrackerRootfsArtifacts,
    pub attached_drives: Vec<FirecrackerAttachedDriveArtifacts>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirecrackerVmStateArtifacts {
    #[serde(skip)]
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirecrackerMemoryArtifacts {
    #[serde(skip)]
    pub image_config_path: PathBuf,
    pub virtual_size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirecrackerRootfsArtifacts {
    #[serde(skip)]
    pub image_config_path: PathBuf,
    pub virtual_size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirecrackerAttachedDriveArtifacts {
    pub drive_id: String,
    pub read_only: bool,
    #[serde(default)]
    pub mount_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_path: Option<PathBuf>,
    pub virtual_size: u64,
    #[serde(skip)]
    pub image_config_path: PathBuf,
}

impl FirecrackerSnapshotManifest {
    pub fn new(
        vm_state_path: impl Into<PathBuf>,
        mem_image_config_path: impl Into<PathBuf>,
        mem_virtual_size: u64,
        rootfs_image_config_path: impl Into<PathBuf>,
        rootfs_virtual_size: u64,
        attached_drives: &[ExtraDrive],
    ) -> Result<Self> {
        Self {
            version: MANIFEST_FORMAT_VERSION,
            delta_empty: None,
            vm_state: Some(FirecrackerVmStateArtifacts {
                path: vm_state_path.into(),
            }),
            memory: Some(FirecrackerMemoryArtifacts {
                image_config_path: mem_image_config_path.into(),
                virtual_size: mem_virtual_size,
            }),
            rootfs: FirecrackerRootfsArtifacts {
                image_config_path: rootfs_image_config_path.into(),
                virtual_size: rootfs_virtual_size,
            },
            attached_drives: Vec::new(),
        }
        .with_extra_drives(attached_drives)
    }

    /// Builds a manifest for a disk-only snapshot: rootfs and attached-drive
    /// state without VM state or memory artifacts. Such snapshots cannot be
    /// resumed; sandboxes launch from them via a fresh (cold) boot.
    pub fn new_disk_only(
        rootfs_image_config_path: impl Into<PathBuf>,
        rootfs_virtual_size: u64,
        attached_drives: &[ExtraDrive],
    ) -> Result<Self> {
        Self {
            version: MANIFEST_FORMAT_VERSION,
            delta_empty: None,
            vm_state: None,
            memory: None,
            rootfs: FirecrackerRootfsArtifacts {
                image_config_path: rootfs_image_config_path.into(),
                virtual_size: rootfs_virtual_size,
            },
            attached_drives: Vec::new(),
        }
        .with_extra_drives(attached_drives)
    }

    /// Whether this snapshot carries resumable VM state (Firecracker vm_state
    /// plus a memory image). Disk-only snapshots return `false` and can only
    /// cold-boot.
    pub fn has_memory_state(&self) -> bool {
        self.vm_state.is_some() && self.memory.is_some()
    }

    pub fn extra_drives(&self) -> Vec<ExtraDrive> {
        self.attached_drives
            .iter()
            .map(|drive| ExtraDrive::Overlaybd {
                drive_id: drive.drive_id.clone(),
                image_config_path: drive.image_config_path.clone(),
                read_only: drive.read_only,
                virtual_size: Some(drive.virtual_size),
                mount_path: crate::sandbox::normalize_mount_path_for_drive(
                    &drive.drive_id,
                    drive.mount_path.clone(),
                )
                .unwrap_or_else(|_| ExtraDrive::default_mount_path(&drive.drive_id)),
                sub_path: drive.sub_path.clone(),
            })
            .collect()
    }

    pub fn with_extra_drives(&self, extra_drives: &[ExtraDrive]) -> Result<Self> {
        let mut new = self.clone();
        new.attached_drives = extra_drives
            .iter()
            .map(|drive| {
                let virtual_size = drive.virtual_size().ok_or_else(|| {
                    anyhow::anyhow!(
                        "snapshot attached drive '{}' virtual size must be known",
                        drive.drive_id()
                    )
                })?;
                if virtual_size == 0 {
                    bail!(
                        "snapshot attached drive '{}' virtual size must be non-zero",
                        drive.drive_id()
                    );
                }
                Ok(FirecrackerAttachedDriveArtifacts {
                    drive_id: drive.drive_id().to_string(),
                    read_only: drive.read_only(),
                    mount_path: drive.mount_path().to_path_buf(),
                    sub_path: drive.sub_path().map(Path::to_path_buf),
                    virtual_size,
                    image_config_path: drive.image_config_path().to_path_buf(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(new)
    }
}

#[cfg(test)]
#[doc(hidden)]
impl FirecrackerSnapshotManifest {
    pub(crate) fn for_test(
        rootfs_virtual_size: u64,
        attached_drives: &[ExtraDrive],
    ) -> FirecrackerSnapshotManifest {
        let mut manifest = FirecrackerSnapshotManifest::new(
            "vm_state.bin",
            "mem_image.json",
            0,
            "rootfs/image.json",
            rootfs_virtual_size,
            attached_drives,
        )
        .expect("test snapshot attached drive virtual size must be known");

        for drive in &mut manifest.attached_drives {
            drive.image_config_path = PathBuf::from("drives")
                .join(&drive.drive_id)
                .join("image.json");
        }

        manifest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_delta_empty_aggregation_preserves_unknown_and_writes() {
        assert_eq!(combine_disk_delta_empty([]), Some(true));
        assert_eq!(
            combine_disk_delta_empty([Some(true), Some(true)]),
            Some(true)
        );
        assert_eq!(combine_disk_delta_empty([Some(true), None]), None);
        assert_eq!(combine_disk_delta_empty([None, Some(false)]), Some(false));
        assert_eq!(combine_disk_delta_empty([Some(false), None]), Some(false));
    }

    #[test]
    fn delta_empty_manifest_round_trip_and_legacy_default() {
        let mut manifest =
            FirecrackerSnapshotManifest::new_disk_only("rootfs/image.json", 8192, &[]).unwrap();
        let old_json = serde_json::to_value(&manifest).unwrap();
        assert!(old_json.get("deltaEmpty").is_none());
        assert_eq!(
            serde_json::from_value::<FirecrackerSnapshotManifest>(old_json)
                .unwrap()
                .delta_empty,
            None
        );
        for flag in [true, false] {
            manifest.delta_empty = Some(flag);
            let json = serde_json::to_value(&manifest).unwrap();
            assert_eq!(json["deltaEmpty"], flag);
            assert_eq!(
                serde_json::from_value::<FirecrackerSnapshotManifest>(json)
                    .unwrap()
                    .delta_empty,
                Some(flag)
            );
        }
    }

    #[test]
    fn attached_drive_virtual_size_is_required() {
        let err = serde_json::from_value::<FirecrackerAttachedDriveArtifacts>(serde_json::json!({
            "driveId": "data",
            "readOnly": true,
            "mountPath": "/mnt/data"
        }))
        .expect_err("attached drive artifact should require virtualSize");

        assert!(err.to_string().contains("virtualSize"));
    }

    #[test]
    fn attached_drive_virtual_size_is_serialized_and_mapped_to_runtime_input() {
        let known = FirecrackerAttachedDriveArtifacts {
            drive_id: "data".to_string(),
            read_only: true,
            mount_path: PathBuf::from("/mnt/data"),
            sub_path: None,
            virtual_size: 4096,
            image_config_path: PathBuf::from("drives/data/image.json"),
        };

        let known_json = serde_json::to_value(&known).unwrap();
        assert_eq!(known_json["virtualSize"], serde_json::json!(4096));

        let manifest = FirecrackerSnapshotManifest {
            version: MANIFEST_FORMAT_VERSION,
            delta_empty: None,
            vm_state: Some(FirecrackerVmStateArtifacts {
                path: PathBuf::from("vm_state.bin"),
            }),
            memory: Some(FirecrackerMemoryArtifacts {
                image_config_path: PathBuf::from("mem_image.json"),
                virtual_size: 4096,
            }),
            rootfs: FirecrackerRootfsArtifacts {
                image_config_path: PathBuf::from("rootfs/image.json"),
                virtual_size: 4096,
            },
            attached_drives: vec![known],
        };

        let drives = manifest.extra_drives();
        assert_eq!(drives[0].virtual_size(), Some(4096));
    }

    #[test]
    fn full_manifest_round_trips_and_reports_memory_state() {
        let manifest = FirecrackerSnapshotManifest::new(
            "vm_state.bin",
            "mem_image.json",
            4096,
            "rootfs/image.json",
            4096,
            &[],
        )
        .expect("full manifest should build");
        assert!(manifest.has_memory_state());

        let json = serde_json::to_value(&manifest).unwrap();
        assert!(json.get("vmState").is_some());
        assert_eq!(json["memory"]["virtualSize"], serde_json::json!(4096));

        let parsed: FirecrackerSnapshotManifest = serde_json::from_value(json).unwrap();
        assert!(parsed.has_memory_state());
    }

    #[test]
    fn disk_only_manifest_omits_vm_state_and_memory() {
        let manifest = FirecrackerSnapshotManifest::new_disk_only("rootfs/image.json", 4096, &[])
            .expect("disk-only manifest should build");
        assert!(!manifest.has_memory_state());

        let json = serde_json::to_value(&manifest).unwrap();
        assert!(json.get("vmState").is_none());
        assert!(json.get("memory").is_none());

        let parsed: FirecrackerSnapshotManifest = serde_json::from_value(json).unwrap();
        assert!(parsed.vm_state.is_none());
        assert!(parsed.memory.is_none());
        assert!(!parsed.has_memory_state());
    }

    #[test]
    fn legacy_manifest_json_with_memory_fields_parses_as_full() {
        // Shape produced by the pre-Option manifest format: `vmState` always
        // serialized as an empty object (its path is #[serde(skip)]) and
        // `memory` with only `virtualSize`.
        let parsed: FirecrackerSnapshotManifest = serde_json::from_value(serde_json::json!({
            "version": 1,
            "vmState": {},
            "memory": { "virtualSize": 1024 },
            "rootfs": { "virtualSize": 2048 },
            "attachedDrives": []
        }))
        .expect("legacy manifest should parse");

        assert!(parsed.has_memory_state());
        assert_eq!(
            parsed.memory.as_ref().map(|memory| memory.virtual_size),
            Some(1024)
        );
    }

    #[test]
    fn new_rejects_attached_drive_without_virtual_size() {
        let drive = ExtraDrive::Overlaybd {
            drive_id: "data".to_string(),
            image_config_path: PathBuf::from("/tmp/data/image.json"),
            read_only: true,
            mount_path: ExtraDrive::default_mount_path("data"),
            virtual_size: None,
            sub_path: None,
        };

        let err = FirecrackerSnapshotManifest::new(
            "vm_state.bin",
            "mem_image.json",
            4096,
            "rootfs/image.json",
            4096,
            &[drive],
        )
        .expect_err("snapshot attached drive virtual size should be required");

        assert!(err.to_string().contains("virtual size must be known"));
    }

    #[test]
    fn with_extra_drives_rejects_zero_virtual_size() {
        let manifest = FirecrackerSnapshotManifest::new(
            "vm_state.bin",
            "mem_image.json",
            4096,
            "rootfs/image.json",
            4096,
            &[],
        )
        .expect("empty attached drives should be valid");
        let drive = ExtraDrive::Overlaybd {
            drive_id: "data".to_string(),
            image_config_path: PathBuf::from("/tmp/data/image.json"),
            read_only: true,
            mount_path: ExtraDrive::default_mount_path("data"),
            virtual_size: Some(0),
            sub_path: None,
        };

        let err = manifest
            .with_extra_drives(&[drive])
            .expect_err("snapshot attached drive virtual size should be non-zero");

        assert!(err.to_string().contains("virtual size must be non-zero"));
    }
}

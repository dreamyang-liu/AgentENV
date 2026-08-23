use super::{handle_status, Client};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
struct SquashSnapshot<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct CreateSnapshot<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(rename = "diskOnly", skip_serializing_if = "std::ops::Not::not")]
    disk_only: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SnapshotInfo {
    #[serde(rename = "snapshotID")]
    pub snapshot_id: String,
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(rename = "imageRef", default, skip_serializing_if = "Option::is_none")]
    pub image_ref: Option<String>,
    #[serde(
        rename = "rootfsLayerCount",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub rootfs_layer_count: Option<u32>,
    #[serde(
        rename = "chainSizeMB",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub chain_size_mb: Option<u64>,
}

impl Client {
    pub fn create_snapshot(
        &self,
        sandbox_id: &str,
        name: Option<&str>,
        disk_only: bool,
    ) -> Result<SnapshotInfo> {
        let body = CreateSnapshot { name, disk_only };
        let resp = handle_status(
            self.post(&format!("/sandboxes/{}/snapshots", sandbox_id))
                .send_json(&body),
        )?;
        Ok(resp.into_json()?)
    }

    pub fn squash_snapshot(&self, snapshot_id: &str, name: Option<&str>) -> Result<SnapshotInfo> {
        let body = SquashSnapshot { name };
        let resp = handle_status(
            self.post(&format!("/snapshots/{}/squash", snapshot_id))
                .send_json(&body),
        )?;
        Ok(resp.into_json()?)
    }

    pub fn list_snapshots(&self, sandbox_id: Option<&str>) -> Result<Vec<SnapshotInfo>> {
        let mut snapshots = Vec::new();
        let mut next_token: Option<String> = None;

        loop {
            let mut request = self.get("/snapshots").query("limit", "100");
            if let Some(sandbox_id) = sandbox_id {
                request = request.query("sandboxID", sandbox_id);
            }
            if let Some(token) = next_token.as_deref() {
                request = request.query("nextToken", token);
            }

            let resp = handle_status(request.call())?;
            next_token = resp
                .header("x-next-token")
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(str::to_string);
            let mut page: Vec<SnapshotInfo> = resp.into_json()?;
            snapshots.append(&mut page);

            if next_token.is_none() {
                break;
            }
        }

        Ok(snapshots)
    }
}

#[cfg(test)]
mod tests {
    use super::{CreateSnapshot, SnapshotInfo};

    #[test]
    fn create_snapshot_serializes_optional_name() {
        let named = serde_json::to_value(CreateSnapshot {
            name: Some("base"),
            disk_only: false,
        })
        .unwrap();
        assert_eq!(named["name"], "base");

        let unnamed = serde_json::to_value(CreateSnapshot {
            name: None,
            disk_only: false,
        })
        .unwrap();
        assert_eq!(unnamed, serde_json::json!({}));

        let disk_only = serde_json::to_value(CreateSnapshot {
            name: None,
            disk_only: true,
        })
        .unwrap();
        assert_eq!(disk_only, serde_json::json!({"diskOnly": true}));
    }

    #[test]
    fn snapshot_info_supports_optional_image_ref() {
        let with_ref: SnapshotInfo = serde_json::from_value(serde_json::json!({
            "snapshotID": "snap-1",
            "names": [],
            "imageRef": "registry.example/ns/app:agentenv-snapshot-snap-1"
        }))
        .unwrap();
        assert_eq!(
            with_ref.image_ref.as_deref(),
            Some("registry.example/ns/app:agentenv-snapshot-snap-1")
        );

        let without_ref: SnapshotInfo = serde_json::from_value(serde_json::json!({
            "snapshotID": "snap-2",
            "names": []
        }))
        .unwrap();
        assert_eq!(without_ref.image_ref, None);
        assert!(serde_json::to_value(without_ref)
            .unwrap()
            .get("imageRef")
            .is_none());
    }
}

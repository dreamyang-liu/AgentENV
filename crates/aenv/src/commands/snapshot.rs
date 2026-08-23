use crate::client::{snapshots::SnapshotInfo, Client};
use crate::output::{self, Format};
use anyhow::Result;
use clap::{Args as ClapArgs, Subcommand};
use tabled::Tabled;

#[derive(ClapArgs)]
#[command(after_help = "Examples:
  aenv snapshot create <sandbox-id> --name my-base
  aenv snapshot ls
  aenv snapshot ls --sandbox-id <sandbox-id>
  aenv snapshot squash <snapshot-id> --name my-base-flat
  aenv start my-base

Snapshots are persistent and reusable. Use `aenv start <snapshot>` to create one or more new sandboxes from a snapshot.")]
pub struct Args {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(Subcommand)]
enum Sub {
    /// Create a persistent snapshot from a running sandbox
    Create {
        sandbox_id: String,
        /// Snapshot name or alias. If omitted, the server returns the generated snapshot ID.
        #[arg(long)]
        name: Option<String>,
        /// Capture disk state only (no VM state / memory). Much cheaper to
        /// store, but sandboxes created from it cold-boot instead of resuming.
        #[arg(long = "disk-only")]
        disk_only: bool,
    },
    /// Merge a snapshot's layer chain into a new, flattened snapshot
    ///
    /// Sandboxes created from a snapshot inherit its layers as an immutable
    /// prefix, so branching from a deep snapshot leaves the child with short
    /// compaction cycles. Squashing publishes an equivalent snapshot whose
    /// chain is a single layer; the source snapshot is left untouched.
    Squash {
        snapshot_id: String,
        /// Alias for the squashed snapshot.
        #[arg(long)]
        name: Option<String>,
    },
    /// List persistent snapshots
    #[command(visible_alias = "ls")]
    List {
        /// Filter snapshots by source sandbox ID
        #[arg(long = "sandbox-id")]
        sandbox_id: Option<String>,
        #[arg(long, value_enum)]
        output: Option<Format>,
    },
}

pub fn run(args: Args) -> Result<()> {
    let client = Client::from_env()?;
    match args.cmd {
        Sub::Create {
            sandbox_id,
            name,
            disk_only,
        } => create(&client, &sandbox_id, name.as_deref(), disk_only),
        Sub::Squash { snapshot_id, name } => squash(&client, &snapshot_id, name.as_deref()),
        Sub::List { sandbox_id, output } => {
            list(&client, sandbox_id.as_deref(), output::resolve(output))
        }
    }
}

#[derive(Tabled)]
struct Row {
    #[tabled(rename = "SNAPSHOT ID")]
    snapshot_id: String,
    #[tabled(rename = "NAMES")]
    names: String,
    #[tabled(rename = "IMAGE REF")]
    image_ref: String,
}

fn create(client: &Client, sandbox_id: &str, name: Option<&str>, disk_only: bool) -> Result<()> {
    let snapshot = client.create_snapshot(sandbox_id, name, disk_only)?;
    println!("Created snapshot {}", snapshot.snapshot_id);
    if let Some(image_ref) = &snapshot.image_ref {
        println!("Image: {image_ref}");
    }
    Ok(())
}

fn squash(client: &Client, snapshot_id: &str, name: Option<&str>) -> Result<()> {
    let snapshot = client.squash_snapshot(snapshot_id, name)?;
    if snapshot.snapshot_id == snapshot_id {
        println!("Snapshot {snapshot_id} chain is already flat; nothing to squash");
        return Ok(());
    }
    println!("Squashed into snapshot {}", snapshot.snapshot_id);
    if let Some(layers) = snapshot.rootfs_layer_count {
        println!("Rootfs layers: {layers}");
    }
    if let Some(size) = snapshot.chain_size_mb {
        println!("Chain size: {size} MiB");
    }
    Ok(())
}

fn list(client: &Client, sandbox_id: Option<&str>, format: Format) -> Result<()> {
    let snapshots = client.list_snapshots(sandbox_id)?;
    output::render(format, &snapshots, |snapshot: &SnapshotInfo| Row {
        snapshot_id: snapshot.snapshot_id.clone(),
        names: if snapshot.names.is_empty() {
            "-".to_string()
        } else {
            snapshot.names.join(",")
        },
        image_ref: snapshot
            .image_ref
            .clone()
            .unwrap_or_else(|| "-".to_string()),
    })
}

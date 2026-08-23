//! Startup command execution for cold-booted snapshot sandboxes.
//!
//! Resuming a full snapshot restores the processes captured in the memory
//! image, so nothing needs to be re-launched. A cold boot from a disk-only
//! snapshot starts a fresh kernel over the captured rootfs instead: any
//! service the snapshot's template started (for example via its `start_cmd`)
//! is gone and must be re-run once the guest is ready.
//!
//! The semantics deliberately mirror the template builder's startup handling
//! (`src/template/runner.rs`): `start_cmd` runs in the background via
//! `/bin/bash -lc` with the startup context's env/cwd, then `ready_cmd` polls
//! until it succeeds within a bounded window.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::Instant;
use tracing::debug;

use envd::process::ProcessClient;

use crate::sandbox::process::{
    run_command_with_client, start_process_with_client, ProcessHandle, ProcessOpts,
};
use crate::snapshot::StartupCommand;

/// Default ready command when a startup command has no explicit ready check.
/// Keep in sync with the template builder's `DEFAULT_READY_WITH_START_CMD`.
const DEFAULT_READY_WITH_START_CMD: &str = "sleep 20";
const READY_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const READY_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Normalizes a persisted startup command for cold boot, mirroring the
/// template builder's `prepare_startup`: drop fully-empty commands and default
/// the ready command when only `start_cmd` is set.
pub(super) fn normalize_startup(startup: Option<StartupCommand>) -> Option<StartupCommand> {
    let mut startup = startup?;
    let start_cmd_empty = startup.start_cmd.trim().is_empty();
    let ready_cmd_empty = startup.ready_cmd.trim().is_empty();
    if start_cmd_empty && ready_cmd_empty {
        return None;
    }
    if !start_cmd_empty && ready_cmd_empty {
        startup.ready_cmd = DEFAULT_READY_WITH_START_CMD.to_string();
    }
    Some(startup)
}

/// Re-runs a snapshot's startup command after a cold boot.
///
/// Takes an owned [`ProcessClient`] (cloned per operation) so the produced
/// future stays `Send`; see `run_command_with_client` for why.
pub(super) async fn run_startup_commands(
    client: ProcessClient,
    startup: &StartupCommand,
) -> Result<()> {
    let mut start_handle = if startup.start_cmd.trim().is_empty() {
        None
    } else {
        debug!(command = %startup.start_cmd, "running cold-boot startup command");
        let handle = start_process_with_client(
            client.clone(),
            "/bin/bash".to_string(),
            vec!["-lc".to_string(), startup.start_cmd.clone()],
            ProcessOpts {
                envs: startup.context.env_vars.clone(),
                cwd: Some(startup.context.workdir.clone()),
                ..ProcessOpts::default()
            },
            false,
        )
        .await
        .with_context(|| format!("execute cold-boot startup command '{}'", startup.start_cmd))?;
        Some(handle)
    };

    if !startup.ready_cmd.trim().is_empty() {
        run_ready_command(client, startup, &mut start_handle).await?;
    }

    if let Some(handle) = start_handle.as_mut() {
        ensure_start_command_still_running_or_success(handle).await?;
    }

    Ok(())
}

async fn run_ready_command(
    client: ProcessClient,
    startup: &StartupCommand,
    start_cmd_handle: &mut Option<ProcessHandle>,
) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut attempt = 0_u64;

    let mut opts = ProcessOpts {
        envs: startup.context.env_vars.clone(),
        cwd: Some(startup.context.workdir.clone()),
        timeout: Some(READY_TIMEOUT),
    };

    loop {
        let now = Instant::now();
        anyhow::ensure!(
            now < deadline,
            "cold-boot ready command timed out after {READY_TIMEOUT:?}: cmd='{}'",
            startup.ready_cmd
        );
        opts.timeout = Some(deadline - now);
        attempt += 1;
        debug!(
            attempt,
            command = %startup.ready_cmd,
            "running cold-boot ready command"
        );

        if let Some(handle) = start_cmd_handle.as_mut() {
            if ensure_start_command_still_running_or_success(handle).await? {
                *start_cmd_handle = None;
            }
        }

        let output = run_command_with_client(
            client.clone(),
            "/bin/bash".to_string(),
            vec!["-lc".to_string(), startup.ready_cmd.clone()],
            opts.clone(),
        )
        .await;

        match output {
            Ok(output) if output.exit_code == 0 => {
                debug!(attempt, "cold-boot ready command succeeded");
                return Ok(());
            }
            Ok(output) => {
                debug!(
                    attempt,
                    exit_code = output.exit_code,
                    "cold-boot ready command not ready"
                );
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "cold-boot ready command timed out after {READY_TIMEOUT:?}: cmd='{}', exit_code={}{}",
                    startup.ready_cmd,
                    output.exit_code,
                    command_output_suffix(&output.stdout, &output.stderr)
                );
            }
            Err(error) => {
                debug!(
                    attempt,
                    error = %format_args!("{error:#}"),
                    "cold-boot ready command failed"
                );
                if Instant::now() >= deadline {
                    return Err(error).with_context(|| {
                        format!(
                            "cold-boot ready command timed out after {READY_TIMEOUT:?}: cmd='{}'",
                            startup.ready_cmd
                        )
                    });
                }
            }
        }

        tokio::time::sleep_until(std::cmp::min(
            Instant::now() + READY_RETRY_INTERVAL,
            deadline,
        ))
        .await;
    }
}

/// Checks whether the background start command has exited.
///
/// Returns `Ok(true)` when it exited successfully (stop watching it),
/// `Ok(false)` when it is still running, and an error when it exited non-zero.
async fn ensure_start_command_still_running_or_success(handle: &mut ProcessHandle) -> Result<bool> {
    match tokio::time::timeout(Duration::from_millis(1), handle.wait()).await {
        Err(_) => Ok(false),
        Ok(Ok(output)) if output.exit_code == 0 => Ok(true),
        Ok(Ok(output)) => anyhow::bail!(
            "cold-boot startup command exited with status {}{}",
            output.exit_code,
            command_output_suffix(&output.stdout, &output.stderr)
        ),
        Ok(Err(error)) => {
            Err(error).context("cold-boot startup command failed while waiting for it")
        }
    }
}

fn command_output_suffix(stdout: &str, stderr: &str) -> String {
    let mut suffix = String::new();
    let stdout = stdout.trim();
    let stderr = stderr.trim();
    if !stdout.is_empty() {
        suffix.push_str(&format!(", stdout: {stdout}"));
    }
    if !stderr.is_empty() {
        suffix.push_str(&format!(", stderr: {stderr}"));
    }
    suffix
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::CommandContext;

    fn startup(start_cmd: &str, ready_cmd: &str) -> StartupCommand {
        StartupCommand {
            start_cmd: start_cmd.to_string(),
            ready_cmd: ready_cmd.to_string(),
            context: CommandContext::default(),
        }
    }

    #[test]
    fn normalize_drops_empty_startup() {
        assert!(normalize_startup(None).is_none());
        assert!(normalize_startup(Some(startup("", ""))).is_none());
        assert!(normalize_startup(Some(startup("  ", "\t"))).is_none());
    }

    #[test]
    fn normalize_defaults_ready_when_start_cmd_is_set() {
        let normalized =
            normalize_startup(Some(startup("./server &", ""))).expect("startup should survive");
        assert_eq!(normalized.start_cmd, "./server &");
        assert_eq!(normalized.ready_cmd, DEFAULT_READY_WITH_START_CMD);
    }

    #[test]
    fn normalize_preserves_explicit_ready_cmd() {
        let normalized = normalize_startup(Some(startup("./server", "curl -sf localhost:3000")))
            .expect("startup should survive");
        assert_eq!(normalized.ready_cmd, "curl -sf localhost:3000");
    }
}

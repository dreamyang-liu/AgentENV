# Ash fork release: v0.1.2-ash.1

This is the `dreamyang-liu/AgentENV` checkpointing fork, based on the 0.1.2 line.
It is not a replacement for the separate upstream/fork v0.1.3 history. Pin the
repository and version below; an unqualified `latest` install selects a different
release. Pre-release images do not update the stable `latest` tags.

## Server installation

Use a dedicated Linux x86_64 or aarch64 host with KVM access (`/dev/kvm`), the
`ublk_drv` kernel module and root/sudo access for host setup. PVM is optional and
requires a compatible x86_64 PVM host; the default KVM bundle does not require PVM.
The server bundles contain the matching server, ublk daemon and runtime dependencies.

On the target host:

```bash
version=v0.1.2-ash.1
curl -fL "https://github.com/dreamyang-liu/AgentENV/releases/download/${version}/install.sh" \
  -o install-agentenv.sh
sudo env AENV_RELEASE_REPO=dreamyang-liu/AgentENV AENV_RELEASE_VERSION="$version" \
  bash install-agentenv.sh
```

Review the installer before running it. It installs binaries, provisions host
permissions and configures the `aenv` systemd service. On an existing installation
it can stop/reconfigure that service; preserve its configuration and data first.
Release asset downloads are checked against GitHub's SHA256 digests. The release
also includes `SHA256SUMS` and `build-info.json` for independent provenance checks.

Inspect `/etc/default/aenv`, `/var/lib/aenv/config/config.toml` and
`systemctl status aenv` after installation. Use the configured API address and
key for health checks; do not expose the API publicly without access controls.
This release does not require rebuilding benchmark images merely to change the
host service version. Disk snapshots still require their referenced layers and
native agent transcripts for application-level branching.

## CLI-only installation

The CLI supports Linux and macOS on x86_64/aarch64. It does not install a VM host
on macOS or create an AgentENV server.

```bash
curl -fL https://github.com/dreamyang-liu/AgentENV/releases/download/v0.1.2-ash.1/install-cli.sh \
  -o install-aenv-cli.sh
AENV_RELEASE_REPO=dreamyang-liu/AgentENV AENV_RELEASE_VERSION=v0.1.2-ash.1 \
  INSTALL_DIR="$HOME/.local/bin" bash install-aenv-cli.sh
"$HOME/.local/bin/aenv" --version
```

## Scope and validation limits

The fork adds snapshot disk-delta metadata, snapshot deletion/squashing and safe
pre-transmission proxy connection retry. It does not replay an already submitted
tool request after a response timeout. Existing snapshot/template stores and
benchmark results are not bundled in this release.

The serial AgentENV library suite passes 771 tests with 4 ignored; the targeted
disk-delta/zeroing test passes. A parallel test with 100ms test-only deadlines can
fail before receiving initial request headers. That known validation limitation
is not presented as a full parallel-suite pass or as proof of production replay.
See the release's build artifacts and exact commit for platform-specific build
status; source publication alone is not proof of a successful installation.

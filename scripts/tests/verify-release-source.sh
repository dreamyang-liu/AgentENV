#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

for script in install.sh install-cli.sh; do
    installer="${repo_root}/scripts/${script}"
    bash -n "$installer"
    selection="$(awk '/^REPO=/ {capture = 1} capture && /^INSTALL_DIR=/ {exit} capture {print}' "$installer")"
    [[ -n "$selection" ]]

    default_url="$(env -u AENV_RELEASE_REPO -u AENV_RELEASE_VERSION bash -c "$selection"$'\nprintf "%s" "$RELEASE_API"')"
    [[ "$default_url" == "https://api.github.com/repos/kvcache-ai/AgentENV/releases/latest" ]]

    pinned_url="$(AENV_RELEASE_REPO=dreamyang-liu/AgentENV AENV_RELEASE_VERSION=v0.1.2-ash.1 \
        bash -c "$selection"$'\nprintf "%s" "$RELEASE_API"')"
    [[ "$pinned_url" == "https://api.github.com/repos/dreamyang-liu/AgentENV/releases/tags/v0.1.2-ash.1" ]]

    if AENV_RELEASE_REPO='../wrong/repo' bash -c "$selection" >/dev/null 2>&1; then
        echo "error: ${script} accepted an invalid repository" >&2
        exit 1
    fi
    if AENV_RELEASE_VERSION='../../latest?redirect=1' bash -c "$selection" >/dev/null 2>&1; then
        echo "error: ${script} accepted an invalid version" >&2
        exit 1
    fi
    echo "${script}: default and pinned release selection validated"
done

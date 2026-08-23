#!/usr/bin/env bash
set -euo pipefail

SUITE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
source "${SUITE_DIR}/../lib/helpers.sh"
init_suite "13_snapshot"

log "Suite: Snapshot Lifecycle"

snapshot_alias="e2e-snapshot-$(date +%s%N)"

# -- Create source sandbox --
source_sandbox_id=$(create_sandbox "$AENV_TEMPLATE_ID" 60); _sync_http
assert_status "$HTTP_STATUS" "201" "create source sandbox"
assert_not_empty "$source_sandbox_id" "source sandbox ID is present"
track_sandbox "$source_sandbox_id"

if wait_for_sandbox_state "$source_sandbox_id" "running" 30; then
  _pass "source sandbox reaches running state"
else
  _fail "source sandbox reaches running state" "running" "timeout"
fi

# -- Capture snapshot from source sandbox --
api_post "/sandboxes/${source_sandbox_id}/snapshots" "$(jq -nc \
  --arg name "$snapshot_alias" \
  '{name: $name}')"
assert_status "$HTTP_STATUS" "201" "POST /sandboxes/{id}/snapshots returns 201"

snapshot_id=$(echo "$HTTP_BODY" | jq -r '.snapshotID // empty')
snapshot_name=$(echo "$HTTP_BODY" | jq -r '.names[0] // empty')
assert_not_empty "$snapshot_id" "snapshotID is present"
assert_not_empty "$snapshot_name" "snapshot name is present"
assert_contains "$snapshot_name" "$snapshot_alias" "snapshot name contains requested alias"
track_template "$snapshot_id"

# -- List snapshots filtered by source sandbox --
api_get "/snapshots?sandboxID=${source_sandbox_id}"
assert_status "$HTTP_STATUS" "200" "GET /snapshots filtered by sandboxID returns 200"

listed_snapshot_count=$(echo "$HTTP_BODY" | jq -r --arg id "$snapshot_id" \
  '[.[] | select(.snapshotID == $id)] | length')
assert_eq "$listed_snapshot_count" "1" "snapshot appears in filtered snapshot list"

# -- Relaunch from snapshot name --
relaunched_sandbox_id=$(create_sandbox "$snapshot_name" 60); _sync_http
assert_status "$HTTP_STATUS" "201" "create sandbox from snapshot name"
assert_not_empty "$relaunched_sandbox_id" "relaunched sandbox ID is present"
track_sandbox "$relaunched_sandbox_id"

if wait_for_sandbox_state "$relaunched_sandbox_id" "running" 30; then
  _pass "sandbox created from snapshot reaches running state"
else
  _fail "sandbox created from snapshot reaches running state" "running" "timeout"
fi

# -- Delete source sandbox; snapshot should remain reusable --
delete_sandbox "$source_sandbox_id"
assert_status "$HTTP_STATUS" "204" "delete source sandbox"

api_get "/sandboxes/${source_sandbox_id}"
assert_status "$HTTP_STATUS" "404" "deleted source sandbox returns 404"

api_get "/snapshots?sandboxID=${source_sandbox_id}"
assert_status "$HTTP_STATUS" "200" "GET /snapshots still works after deleting source sandbox"
listed_after_delete=$(echo "$HTTP_BODY" | jq -r --arg id "$snapshot_id" \
  '[.[] | select(.snapshotID == $id)] | length')
assert_eq "$listed_after_delete" "1" "snapshot remains listed after deleting source sandbox"

# -- Snapshot remains launchable after source sandbox deletion --
reused_sandbox_id=$(create_sandbox "$snapshot_name" 60); _sync_http
assert_status "$HTTP_STATUS" "201" "create second sandbox from snapshot after source deletion"
assert_not_empty "$reused_sandbox_id" "reused sandbox ID is present"
track_sandbox "$reused_sandbox_id"

if wait_for_sandbox_state "$reused_sandbox_id" "running" 30; then
  _pass "snapshot remains launchable after source sandbox deletion"
else
  _fail "snapshot remains launchable after source sandbox deletion" "running" "timeout"
fi

# -- Disk-only snapshot: capture without memory, relaunch cold-boots --
disk_only_alias="e2e-disk-only-$(date +%s%N)"
disk_only_source_id=$(create_sandbox "$AENV_TEMPLATE_ID" 60); _sync_http
assert_status "$HTTP_STATUS" "201" "create disk-only source sandbox"
track_sandbox "$disk_only_source_id"

if wait_for_sandbox_state "$disk_only_source_id" "running" 30; then
  _pass "disk-only source sandbox reaches running state"
else
  _fail "disk-only source sandbox reaches running state" "running" "timeout"
fi

api_post "/sandboxes/${disk_only_source_id}/snapshots" "$(jq -nc \
  --arg name "$disk_only_alias" \
  '{name: $name, diskOnly: true}')"
assert_status "$HTTP_STATUS" "201" "POST snapshots with diskOnly returns 201"

disk_only_snapshot_id=$(echo "$HTTP_BODY" | jq -r '.snapshotID // empty')
assert_not_empty "$disk_only_snapshot_id" "disk-only snapshotID is present"
track_template "$disk_only_snapshot_id"

# The source keeps running after a disk-only capture.
api_get "/sandboxes/${disk_only_source_id}"
assert_status "$HTTP_STATUS" "200" "disk-only source sandbox still queryable"

# Creating from a disk-only snapshot cold-boots a fresh kernel over the
# captured disk state (no memory image exists to resume).
cold_boot_sandbox_id=$(create_sandbox "$disk_only_alias" 60); _sync_http
assert_status "$HTTP_STATUS" "201" "create sandbox from disk-only snapshot"
assert_not_empty "$cold_boot_sandbox_id" "cold-boot sandbox ID is present"
track_sandbox "$cold_boot_sandbox_id"

if wait_for_sandbox_state "$cold_boot_sandbox_id" "running" 60; then
  _pass "cold-booted sandbox reaches running state"
else
  _fail "cold-booted sandbox reaches running state" "running" "timeout"
fi

delete_sandbox "$cold_boot_sandbox_id"
assert_status "$HTTP_STATUS" "204" "delete cold-booted sandbox"

delete_sandbox "$disk_only_source_id"
assert_status "$HTTP_STATUS" "204" "delete disk-only source sandbox"

api_delete "/templates/${disk_only_snapshot_id}"
assert_status "$HTTP_STATUS" "204" "delete disk-only snapshot returns 204"

# -- Cleanup relaunched sandboxes and snapshot template explicitly --
delete_sandbox "$relaunched_sandbox_id"
assert_status "$HTTP_STATUS" "204" "delete first sandbox created from snapshot"

delete_sandbox "$reused_sandbox_id"
assert_status "$HTTP_STATUS" "204" "delete second sandbox created from snapshot"

api_delete "/templates/${snapshot_id}"
assert_status "$HTTP_STATUS" "204" "delete snapshot template returns 204"

suite_summary "13_snapshot"

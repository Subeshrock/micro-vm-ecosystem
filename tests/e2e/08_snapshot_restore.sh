#!/bin/bash
set -e
source tests/e2e/common.sh

echo "=== Test 08: Snapshot & Restore ==="

check_root
setup_env

echo "Starting Daemon (3008)..."
sudo -E $VYOMAD_BIN --data-dir $TEST_HOME/.vyoma --socket-path /run/vyoma/test.sock --http-port 3008 > $TEST_HOME/daemon.log 2>&1 &
DAEMON_PID=$!
sleep 3

VYOMA="$VYOMA_BIN --socket-path /run/vyoma/test.sock --http-port 3008"

MARKER_FILE="/tmp/snapshot_test_marker"
rm -f "$MARKER_FILE"

echo "Running long-lived VM with sleep..."
VM_OUTPUT=$($VYOMA run alpine:latest --hostname snap-vm --vcpu 1 --memory 128 2>&1) || { echo "Run failed: $VM_OUTPUT"; exit 1; }
VM_ID=$(echo "$VM_OUTPUT" | awk -F 'VM ID: ' '{print $2}' | awk '{print $1}' | tr -d ',')

if [ -z "$VM_ID" ]; then
    VM_ID=$($VYOMA ps | grep "snap-vm" | awk '{print $1}')
fi

echo "VM ID: $VM_ID"

if [ -z "$VM_ID" ]; then
    echo -e "${RED}Fail: Could not start VM${NC}"
    exit 1
fi

register_vm "$VM_ID"

wait_for_vm_state "$VM_ID" "Running" 15
assert_success "VM started"

sleep 2

echo "Taking snapshot..."
SNAP_OUTPUT=$($VYOMA snapshot "$VM_ID" 2>&1)
echo "$SNAP_OUTPUT"
assert_success "Snapshot created"

SNAP_ID=$(echo "$SNAP_OUTPUT" | grep -oP 'snap-[a-f0-9]+' | head -1)
if [ -z "$SNAP_ID" ]; then
    SNAP_ID="snap-0"
fi
echo "Snapshot ID: $SNAP_ID"

echo "Listing history..."
$VYOMA history "$VM_ID"

echo "Deleting VM and restoring from snapshot..."
$VYOMA stop "$VM_ID" 2>/dev/null || true
unregister_vm "$VM_ID"
sleep 1

echo "Time-travel to snapshot..."
RESTORE_OUTPUT=$($VYOMA time-travel "$VM_ID" --to "$SNAP_ID" 2>&1)
echo "$RESTORE_OUTPUT"
assert_success "Time-travel restore"

sleep 2

echo "Verify VM is running after restore..."
VM_AFTER=$($VYOMA ps | grep "snap-vm" | awk '{print $1}')
if [ -n "$VM_AFTER" ]; then
    echo -e "${GREEN}Pass: VM restored from snapshot${NC}"
    register_vm "$VM_AFTER"
else
    echo -e "${RED}Fail: VM not found after restore${NC}"
    exit 1
fi

echo "Cleaning up..."
$VYOMA stop "$VM_ID" 2>/dev/null || true

cleanup_env $DAEMON_PID
echo "=== Test 08 Passed ==="
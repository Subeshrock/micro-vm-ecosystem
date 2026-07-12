#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

echo "=== Test 13: VM Stats ==="

# Setup environment
check_root
setup_env
export RUST_LOG=info

echo "Starting Daemon..."
sudo -E $VYOMAD_BIN --data-dir $TEST_HOME/.vyoma --keep-root --socket-path /run/vyoma/test.sock --http-port 3002 > $TEST_HOME/daemon.log 2>&1 &
DAEMON_PID=$!
sleep 3

VYOMA="$VYOMA_BIN --socket-path /run/vyoma/test.sock --http-port 3002"

echo "Pulling image..."
$VYOMA pull docker.io/library/nginx:alpine || {
    echo "Pull failed. Daemon log:"
    cat $TEST_HOME/daemon.log
    exit 1
}

echo "Running VM..."
VM_ID=$(vyoma_run_and_get_id docker.io/library/nginx:alpine --hostname stats-vm --memory 128)

echo "Starting busy loop inside the VM via exec..."
$VYOMA exec $VM_ID -- sh -c "while true; do true; done" &
EXEC_PID=$!

sleep 5

echo "Fetching VM stats..."
if [ -z "$VM_ID" ]; then
    echo "Failed to get VM ID"
    cleanup_env $DAEMON_PID
    exit 1
fi

echo "Fetching VM stats (first time to prime the CPU measurement)..."
$VYOMA stats $VM_ID --no-stream > /dev/null || true
sleep 2

echo "Fetching VM stats (second time for actual measurement)..."
STATS_OUT=$($VYOMA stats $VM_ID --no-stream)

echo "$STATS_OUT"

SHORT_VM_ID=${VM_ID:0:12}
if ! echo "$STATS_OUT" | grep -q "$SHORT_VM_ID"; then
    echo "Stats output did not contain VM ID"
    cleanup_env $DAEMON_PID
    exit 1
fi

# Assert CPU is non-zero (since it's a busy loop, it should definitely use CPU)
# Stats format: VM_ID     100.00%   1.00MiB / 128.00MiB   0.78%
CPU_PCT=$(echo "$STATS_OUT" | tail -n 1 | awk '{print $2}' | tr -d '%')
if (( $(echo "$CPU_PCT == 0.00" | bc -l) )); then
    echo "CPU usage is 0.00%, expected > 0 for a busy loop"
    cleanup_env $DAEMON_PID
    exit 1
fi

echo "Stats output looks correct."

echo "Cleaning up..."
$VYOMA stop $VM_ID
kill $EXEC_PID || true

cleanup_env $DAEMON_PID

echo "=== Test 13 passed ==="

#!/bin/bash
set -e
source tests/e2e/common.sh

echo "=== Test 12: Exec Command ==="

check_root
setup_env

# Start Daemon
echo "Starting Daemon..."
sudo -E $VYOMAD_BIN --keep-root --socket-path /run/vyoma/test.sock --http-port 3001 > $TEST_HOME/daemon.log 2>&1 &
DAEMON_PID=$!
sleep 3

VYOMA="$VYOMA_BIN --socket-path /run/vyoma/test.sock --http-port 3001"

echo "Pulling image..."
$VYOMA pull docker.io/library/nginx:alpine || {
    echo "Pull failed. Daemon log:"
    cat $TEST_HOME/daemon.log
    exit 1
}

echo "Running VM..."
VM_ID=$(vyoma_run_and_get_id docker.io/library/nginx:alpine --hostname exec-test)
sleep 15 # Wait for VM and agent to start

echo "Executing command in VM..."
OUTPUT=$($VYOMA exec $VM_ID echo "Hello from VM" 2>&1) || true

if echo "$OUTPUT" | grep -q "Hello from VM"; then
    echo "Exec command succeeded"
else
    echo "Exec command failed. Output:"
    echo "$OUTPUT"
    
    echo "Daemon Log:"
    cat $TEST_HOME/daemon.log
    
    cleanup_env $DAEMON_PID
    exit 1
fi

echo "Cleaning up..."
$VYOMA stop $VM_ID || true
cleanup_env $DAEMON_PID

echo "=== Test 12 passed ==="

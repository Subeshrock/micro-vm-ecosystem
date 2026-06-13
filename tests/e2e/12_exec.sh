#!/bin/bash
set -e
source tests/e2e/common.sh

echo "=== Test 12: Exec Command ==="

check_root
setup_env

# Start Daemon
echo "Starting Daemon..."
sudo -E $VYOMAD_BIN --socket-path /run/vyoma/test.sock --http-port 3001 > $TEST_HOME/daemon.log 2>&1 &
DAEMON_PID=$!
sleep 3

VYOMA="$VYOMA_BIN --socket-path /run/vyoma/test.sock --http-port 3001"

echo "Pulling image..."
sudo -E $VYOMA pull docker.io/library/alpine:latest

echo "Running VM..."
VM_ID=$(sudo -E $VYOMA run -d --name exec-test docker.io/library/alpine:latest sh -c "sleep 3600")
sleep 5 # Wait for VM and agent to start

echo "Executing command in VM..."
OUTPUT=$(sudo -E $VYOMA exec exec-test echo "Hello from VM")

if echo "$OUTPUT" | grep -q "Hello from VM"; then
    echo "Exec command succeeded"
else
    echo "Exec command failed. Output:"
    echo "$OUTPUT"
    
    echo "Daemon Log:"
    cat $TEST_HOME/daemon.log
    
    sudo -E $VYOMA rm -f exec-test
    cleanup
    exit 1
fi

echo "Cleaning up..."
sudo -E $VYOMA rm -f exec-test
cleanup

echo "=== Test 12 passed ==="

#!/bin/bash
set -e
source "$(dirname "$0")/common.sh"

echo "Running 14_volumes.sh..."

check_root
setup_env

mkdir -p "$TEST_HOME/host_vol"
echo "Hello from host" > "$TEST_HOME/host_vol/hello.txt"
chmod 777 "$TEST_HOME/host_vol"

# Start Daemon
echo "Starting Daemon..."
sudo -E $VYOMAD_BIN --data-dir $TEST_HOME/.vyoma --socket-path /run/vyoma/test.sock --http-port 3014 > $TEST_HOME/daemon.log 2>&1 &
DAEMON_PID=$!
sleep 3

VYOMA="$VYOMA_BIN --socket-path /run/vyoma/test.sock --http-port 3014"

echo "Running VM with volume mount..."
$VYOMA run -v "$TEST_HOME/host_vol:/mnt" alpine:latest --hostname vol-test

sleep 2
VM_ID=$($VYOMA ps | grep "vol-test" | awk '{print $1}')

echo "Started VM: $VM_ID"

if [ -z "$VM_ID" ]; then
    echo "ERROR: Failed to get VM ID"
    cleanup_env
    exit 1
fi

sleep 5

echo "Testing file readability in VM..."
OUTPUT=$(RUST_LOG=error $VYOMA exec "$VM_ID" cat /mnt/hello.txt)

if [ "$OUTPUT" != "Hello from host" ]; then
    echo "ERROR: File content mismatch. Expected 'Hello from host', got: $OUTPUT"
    cleanup_env 14
    exit 1
fi

echo "File content match!"

echo "Testing write from VM to host..."
RUST_LOG=error $VYOMA exec "$VM_ID" sh -c "echo 'Hello from VM' > /mnt/vm.txt"

if [ ! -f "$TEST_HOME/host_vol/vm.txt" ]; then
    echo "ERROR: File vm.txt not found on host!"
    cleanup_env 14
    exit 1
fi

HOST_READ=$(cat "$TEST_HOME/host_vol/vm.txt")
if [ "$HOST_READ" != "Hello from VM" ]; then
    echo "ERROR: File content mismatch on host. Expected 'Hello from VM', got: $HOST_READ"
    cleanup_env 14
    exit 1
fi

echo "Host read matched VM write!"

echo "Cleaning up..."

cleanup_env
echo "14_volumes.sh completed successfully."

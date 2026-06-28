#!/bin/bash
set -e
source tests/e2e/common.sh

echo "=== Test 10: Volume Mount ==="

check_root
setup_env

echo "Starting Daemon (3010)..."
HOME=$TEST_HOME sudo -E $VYOMAD_BIN --data-dir $TEST_HOME/.vyoma --socket-path /run/vyoma/test.sock --http-port 3010 > $TEST_HOME/daemon.log 2>&1 &
DAEMON_PID=$!
sleep 3

VYOMA="$VYOMA_BIN --socket-path /run/vyoma/test.sock --http-port 3010"

HOST_DIR="$TEST_HOME/volume_data"
mkdir -p "$HOST_DIR"
echo "vyoma-test-data" > "$HOST_DIR/testfile.txt"

# Ensure the directory is readable/writable by the vyoma user
chmod o+rwx "$HOST_DIR"
chmod o+rw "$HOST_DIR/testfile.txt"

echo "Running VM with volume mount -v ${HOST_DIR}:/data..."
set +e
VM_OUTPUT=$($VYOMA run --hostname vol-test -v "${HOST_DIR}:/data" --vcpu 1 --memory 128 docker.io/library/nginx:alpine 2>&1)
RUN_EXIT=$?
set -e
echo "Run exit code: $RUN_EXIT"
echo "Run output: $VM_OUTPUT"
VM_ID=$(echo "$VM_OUTPUT" | grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' | tail -1 | tr -d '[:space:]')

if [ -z "$VM_ID" ]; then
    VM_ID=$($VYOMA ps | grep "vol-test" | awk '{print $1}')
fi

echo "VM ID: $VM_ID"

if [ -z "$VM_ID" ]; then
    echo -e "${RED}Fail: Could not start VM${NC}"
    exit 1
fi

register_vm "$VM_ID"
wait_for_vm_state "$VM_ID" "Running" 15
assert_success "VM started with volume"

# Wait a little for the agent to start
sleep 2

echo "[TEST] Reading file from volume mount via exec"
set +e
OUTPUT=$($VYOMA exec "$VM_ID" cat /data/testfile.txt 2>&1)
EXIT_CODE=$?
set -e

echo "Exec exit code: $EXIT_CODE"
echo "Exec output: $OUTPUT"

if [[ "$OUTPUT" != *"vyoma-test-data"* ]]; then
    echo -e "${RED}FAIL: Expected 'vyoma-test-data', got '$OUTPUT'${NC}"
    exit 1
else
    echo -e "${GREEN}Pass: Volume mount works - file content visible via exec${NC}"
fi

echo "Cleaning up..."
$VYOMA stop "$VM_ID" 2>/dev/null || true
unregister_vm "$VM_ID"
cleanup_env $DAEMON_PID
echo "=== Test 10 Passed ==="
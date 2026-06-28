#!/bin/bash
set -e
source tests/e2e/common.sh

echo "=== Test 09: Port Forwarding ==="

check_root
setup_env

echo "Starting Daemon (3009)..."
sudo -E $VYOMAD_BIN --data-dir $TEST_HOME/.vyoma --socket-path /run/vyoma/test.sock --http-port 3009 > $TEST_HOME/daemon.log 2>&1 &
DAEMON_PID=$!
sleep 3

VYOMA="$VYOMA_BIN --socket-path /run/vyoma/test.sock --http-port 3009"

HOST_PORT=18080

echo "Running nginx VM with port mapping -p ${HOST_PORT}:80..."
VM_OUTPUT=$($VYOMA run nginx:latest --hostname nginx-port-test --port "${HOST_PORT}:80" -- vcpu 1 --memory 256 -- sleep 300 2>&1)
VM_ID=$(echo "$VM_OUTPUT" | awk -F 'VM ID: ' '{print $2}' | awk '{print $1}' | tr -d ',')

if [ -z "$VM_ID" ]; then
    VM_ID=$($VYOMA ps | grep "nginx-port-test" | awk '{print $1}')
fi

echo "VM ID: $VM_ID"

if [ -z "$VM_ID" ]; then
    echo -e "${RED}Fail: Could not start VM${NC}"
    exit 1
fi

register_vm "$VM_ID"

wait_for_vm_state "$VM_ID" "Running" 15
assert_success "VM started"

echo "Waiting for port ${HOST_PORT} to be open..."
wait_for_port "$HOST_PORT" 30
assert_success "Port forwarding active"

echo "Testing HTTP response..."
HTTP_RESPONSE=$(curl -s "http://localhost:${HOST_PORT}" 2>&1 || curl -s "http://127.0.0.1:${HOST_PORT}" 2>&1)

if echo "$HTTP_RESPONSE" | grep -qi "nginx"; then
    echo -e "${GREEN}Pass: Port forwarding works - nginx responding${NC}"
else
    echo -e "${RED}Fail: Expected nginx response, got: $HTTP_RESPONSE${NC}"
    exit 1
fi

echo "Cleaning up..."
$VYOMA stop "$VM_ID" 2>/dev/null || true
unregister_vm "$VM_ID"

cleanup_env $DAEMON_PID
echo "=== Test 09 Passed ==="
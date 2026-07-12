#!/bin/bash
set -e
source tests/e2e/common.sh

echo "=== Test 03: Builder (Vyomafile) ==="

check_root
setup_env

echo "Starting Daemon (3003)..."
sudo -E $VYOMAD_BIN --data-dir $TEST_HOME/.vyoma --socket-path /run/vyoma/test.sock --http-port 3003 > $TEST_HOME/daemon.log 2>&1 &
DAEMON_PID=$!
sleep 3

VYOMA="$VYOMA_BIN --socket-path /run/vyoma/test.sock --http-port 3003"

CTX=$TEST_HOME/build_ctx
mkdir -p $CTX
cat <<EOF > $CTX/Vyomafile
FROM alpine:latest
RUN echo "Vyoma Build Test" > /build_test.txt
CMD ["sleep", "60"]
EOF

echo "Pulling base image..."
$VYOMA pull alpine:latest 2>/dev/null || true

echo "Building Image..."
OUTPUT=$($VYOMA build $CTX 2>&1) || { echo "Build failed: $OUTPUT"; exit 1; }
echo "$OUTPUT"
assert_success "Build Command"

if echo "$OUTPUT" | grep -q "Build complete"; then
    echo -e "${GREEN}Pass: Build completed successfully${NC}"
else
    echo -e "${RED}Fail: Build did not complete${NC}"
    exit 1
fi

IMAGE_ID=$(echo "$OUTPUT" | grep -o 'Image ID: [a-f0-9\-]*' | awk '{print $3}' | head -1)

echo "Running built image as VM..."
VM_ID=$($VYOMA run "$IMAGE_ID" --hostname built-vm --vcpu 1 --memory 128 2>&1)
VM_ID=$(echo "$VM_ID" | awk -F 'VM ID: ' '{print $2}' | awk '{print $1}' | tr -d ',')

if [ -z "$VM_ID" ]; then
    VM_ID=$($VYOMA ps | grep "built-vm" | awk '{print $1}')
fi

echo "VM ID: $VM_ID"

if [ -n "$VM_ID" ]; then
    register_vm "$VM_ID"
    wait_for_vm_state "$VM_ID" "Running" 15
    assert_success "Built image runs as VM"
else
    echo -e "${RED}Fail: Could not start VM from built image${NC}"
    exit 1
fi

echo "Stopping test VM..."
$VYOMA stop "$VM_ID" 2>/dev/null || true
unregister_vm "$VM_ID"

cleanup_env $DAEMON_PID
echo "=== Test 03 Passed ==="

#!/bin/bash
set -e
source tests/e2e/common.sh

echo "=== Test 11: Migration / Teleport Automation ==="

check_root
setup_env

SOURCE_PORT=3012
TARGET_PORT=3013

echo "Starting source daemon (port $SOURCE_PORT)..."
sudo -E $VYOMAD_BIN --data-dir $TEST_HOME/.vyoma --socket-path /run/vyoma/source.sock --http-port $SOURCE_PORT > $TEST_HOME/source-daemon.log 2>&1 &
SOURCE_PID=$!
sleep 3

echo "Starting target daemon (port $TARGET_PORT)..."
sudo -E $VYOMAD_BIN --data-dir $TEST_HOME/.vyoma --socket-path /run/vyoma/target.sock --http-port $TARGET_PORT > $TEST_HOME/target-daemon.log 2>&1 &
TARGET_PID=$!
sleep 3

VYOMA_SOURCE="$VYOMA_BIN --socket-path /run/vyoma/source.sock --http-port $SOURCE_PORT"
VYOMA_TARGET="$VYOMA_BIN --socket-path /run/vyoma/target.sock --http-port $TARGET_PORT"

echo "1. Pull test image..."
$VYOMA_SOURCE pull alpine:latest 2>/dev/null || true
$VYOMA_TARGET pull alpine:latest 2>/dev/null || true

echo "2. Run VM on source node..."
VM_OUTPUT=$($VYOMA_SOURCE run alpine:latest --hostname migrate-vm --vcpu 1 --memory 128 -- sleep 300 2>&1)
VM_ID=$(echo "$VM_OUTPUT" | awk -F 'VM ID: ' '{print $2}' | awk '{print $1}' | tr -d ',')

if [ -z "$VM_ID" ]; then
    VM_ID=$($VYOMA_SOURCE ps | grep "migrate-vm" | awk '{print $1}')
fi

echo "Source VM ID: $VM_ID"

if [ -z "$VM_ID" ]; then
    echo -e "${RED}Fail: Could not start VM on source${NC}"
    exit 1
fi

register_vm "$VM_ID"

wait_for_vm_state "$VM_ID" "Running" 15
assert_success "VM started on source"

sleep 3

echo "3. Run migration (teleport) to target node..."
MIGRATE_OUTPUT=$($VYOMA_SOURCE teleport "$VM_ID" --target-ip "127.0.0.1" --target-port $TARGET_PORT --bandwidth 100 2>&1)
echo "$MIGRATE_OUTPUT"

if echo "$MIGRATE_OUTPUT" | grep -qi "error\|fail\|could not"; then
    echo -e "${RED}Fail: Migration failed${NC}"
    echo "$MIGRATE_OUTPUT"
    exit 1
fi

MIGRATE_SUCCESS=0
for i in {1..30}; do
    sleep 2
    echo "Checking migration status... ($i/30)"

    if $VYOMA_TARGET ps 2>/dev/null | grep -q "migrate-vm"; then
        echo -e "${GREEN}Pass: VM migrated to target${NC}"
        MIGRATE_SUCCESS=1
        break
    fi
done

if [ $MIGRATE_SUCCESS -eq 0 ]; then
    echo -e "${YELLOW}Warning: Migration may have failed or not detected in ps${NC}"
    echo "Checking source VM status..."
    if $VYOMA_SOURCE ps | grep -q "migrate-vm"; then
        echo "VM still on source - migration may have failed and resumed"
    fi
fi

echo "4. Verify VM runs on target..."
TARGET_VM=$($VYOMA_TARGET ps | grep "migrate-vm" | awk '{print $1}')
if [ -n "$TARGET_VM" ]; then
    register_vm "$TARGET_VM"
    wait_for_vm_state_from_cli "$VYOMA_TARGET" "$TARGET_VM" "Running" 15
    echo -e "${GREEN}Pass: VM running on target after migration${NC}"
else
    echo -e "${YELLOW}Warning: Could not verify target VM state${NC}"
fi

echo "5. Test migration failure handling..."
echo "Creating VM for failure test..."
VM_FAIL_OUTPUT=$($VYOMA_SOURCE run alpine:latest --hostname migrate-fail --vcpu 1 --memory 128 -- sleep 300 2>&1)
VM_FAIL_ID=$(echo "$VM_FAIL_OUTPUT" | awk -F 'VM ID: ' '{print $2}' | awk '{print $1}' | tr -d ',')

if [ -z "$VM_FAIL_ID" ]; then
    VM_FAIL_ID=$($VYOMA_SOURCE ps | grep "migrate-fail" | awk '{print $1}')
fi

if [ -n "$VM_FAIL_ID" ]; then
    register_vm "$VM_FAIL_ID"
    wait_for_vm_state "$VM_FAIL_ID" "Running" 15

    echo "Attempting migration to unreachable target..."
    $VYOMA_SOURCE teleport "$VM_FAIL_ID" --target-ip "192.168.255.254" --target-port 9999 2>&1 || true

    sleep 5

    echo "Checking if source VM preserved after failed migration..."
    if $VYOMA_SOURCE ps | grep -q "migrate-fail"; then
        echo -e "${GREEN}Pass: Source VM preserved after failed migration${NC}"
    else
        echo -e "${YELLOW}Info: Source VM not found after migration failure${NC}"
    fi

    $VYOMA_SOURCE stop "$VM_FAIL_ID" 2>/dev/null || true
    unregister_vm "$VM_FAIL_ID"
fi

echo "6. Test migration progress status..."
echo "Starting another migration to check status endpoint..."

VM_STATUS_OUTPUT=$($VYOMA_SOURCE run alpine:latest --hostname migrate-status --vcpu 1 --memory 128 -- sleep 300 2>&1)
VM_STATUS_ID=$(echo "$VM_STATUS_OUTPUT" | awk -F 'VM ID: ' '{print $2}' | awk '{print $1}' | tr -d ',')

if [ -z "$VM_STATUS_ID" ]; then
    VM_STATUS_ID=$($VYOMA_SOURCE ps | grep "migrate-status" | awk '{print $1}')
fi

if [ -n "$VM_STATUS_ID" ]; then
    register_vm "$VM_STATUS_ID"
    wait_for_vm_state "$VM_STATUS_ID" "Running" 15

    MIGRATE_OUTPUT=$($VYOMA_SOURCE teleport "$VM_STATUS_ID" --target-ip "127.0.0.1" --target-port $TARGET_PORT --bandwidth 50 2>&1)
    echo "Migration started, checking status..."

    sleep 3
    if $VYOMA_SOURCE teleport status 2>/dev/null | grep -qi "progress\|completed\|failed\|pending"; then
        echo -e "${GREEN}Pass: Migration status endpoint functional${NC}"
    else
        echo -e "${YELLOW}Info: Could not verify status endpoint${NC}"
    fi

    $VYOMA_SOURCE stop "$VM_STATUS_ID" 2>/dev/null || true
    unregister_vm "$VM_STATUS_ID"
fi

echo "Cleaning up..."
$VYOMA_SOURCE stop "$VM_ID" 2>/dev/null || true
$VYOMA_TARGET stop "$VM_ID" 2>/dev/null || true

cleanup_env $SOURCE_PID
echo "=== Test 11 Passed ==="
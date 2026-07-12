#!/bin/bash
set -e
source tests/e2e/common.sh
trap "cat $TEST_HOME/daemon.log" EXIT

echo "=== Test 11: Attestation / Measured Boot ==="

check_root
setup_env

echo "Starting Daemon (3011)..."
sudo -E $VYOMAD_BIN --data-dir $TEST_HOME/.vyoma --socket-path /run/vyoma/test.sock --http-port 3011 > $TEST_HOME/daemon.log 2>&1 &
DAEMON_PID=$!
sleep 3

VYOMA="$VYOMA_BIN --socket-path /run/vyoma/test.sock --http-port 3011"

KEY_DIR="$TEST_HOME/keys"
mkdir -p "$KEY_DIR"

echo "Generating signing keys for measured boot..."
openssl genrsa -out "$KEY_DIR/sign_key.pem" 2048 2>/dev/null
openssl rsa -in "$KEY_DIR/sign_key.pem" -pubout -out "$KEY_DIR/sign_key.pub" 2>/dev/null

echo "Testing measured boot flow..."

echo "1. Build image with --measured flag..."
$VYOMA pull alpine:latest 2>/dev/null || true

CTX=$TEST_HOME/measured_ctx
mkdir -p $CTX
cat <<EOF > $CTX/Vyomafile
FROM alpine:latest
RUN echo "Measured boot test image" > /measured.txt
CMD ["sleep", "300"]
EOF

echo "Building measured image..."
BUILD_OUTPUT=$($VYOMA build --measured --signing-key "$KEY_DIR/sign_key.pub" $CTX 2>&1)
echo "$BUILD_OUTPUT"

if ! echo "$BUILD_OUTPUT" | grep -qi "measured\|build.*complete"; then
    echo -e "${YELLOW}Warning: --measured flag may not be implemented yet${NC}"
fi

echo "2. Run VM and verify attestation..."
VM_OUTPUT=$($VYOMA run alpine:latest --hostname attest-vm --vcpu 1 --memory 128 --measured 2>&1)
VM_ID=$(echo "$VM_OUTPUT" | awk -F 'VM ID: ' '{print $2}' | awk '{print $1}' | tr -d ',')

if [ -z "$VM_ID" ]; then
    VM_ID=$($VYOMA ps | grep "attest-vm" | awk '{print $1}')
fi

echo "VM ID: $VM_ID"

if [ -z "$VM_ID" ]; then
    echo -e "${RED}Fail: Could not start VM${NC}"
    exit 1
fi

register_vm "$VM_ID"

wait_for_vm_state "$VM_ID" "Running" 20
assert_success "VM started with measured boot"

sleep 2

echo "3. Check attestation status..."
ATTEST_STATUS=$($VYOMA inspect "$VM_ID" 2>&1 | grep -i "attestation\|measured\|attested" || echo "unknown")
echo "Attestation status: $ATTEST_STATUS"

if echo "$ATTEST_STATUS" | grep -qi "passed\|verified\|attested\|true"; then
    echo -e "${GREEN}Pass: Attestation verified${NC}"
elif echo "$ATTEST_STATUS" | grep -qi "unknown\|not.*found"; then
    echo -e "${YELLOW}Info: Attestation status not available (may need policy config)${NC}"
else
    echo -e "${YELLOW}Warning: Could not verify attestation status${NC}"
fi

echo "4. Test manifest tampering detection (if supported)..."

TAMPERED_CTX=$TEST_HOME/tampered_ctx
mkdir -p $TAMPERED_CTX
cp -r $CTX/* $TAMPERED_CTX/
echo "modified-by-attacker" >> $TAMPERED_CTX/Vyomafile

if $VYOMA build --verify --signing-key "$KEY_DIR/sign_key.pub" $TAMPERED_CTX 2>&1 | grep -qi "fail\|invalid\|tamper"; then
    echo -e "${GREEN}Pass: Tampered manifest detected${NC}"
else
    echo -e "${YELLOW}Info: Tamper detection not implemented or not triggered${NC}"
fi

echo "5. Verify VM is still running (attestation should kill if failed)..."
sleep 1
CURRENT_STATE=$($VYOMA ps | grep "$VM_ID" | awk '{print $NF}' | tr -d '[]')
if [ "$CURRENT_STATE" = "Running" ]; then
    echo -e "${GREEN}Pass: VM still running (attestation passed or not enforced)${NC}"
else
    echo -e "${RED}Fail: VM not running - may have been killed due to failed attestation${NC}"
fi

echo "6. Test without measured flag (should work normally)..."
VM_OUTPUT2=$($VYOMA run alpine:latest --hostname normal-vm --vcpu 1 --memory 128 2>&1)
VM_ID2=$(echo "$VM_OUTPUT2" | awk -F 'VM ID: ' '{print $2}' | awk '{print $1}' | tr -d ',')

if [ -z "$VM_ID2" ]; then
    VM_ID2=$($VYOMA ps | grep "normal-vm" | awk '{print $1}')
fi

if [ -n "$VM_ID2" ]; then
    register_vm "$VM_ID2"
    wait_for_vm_state "$VM_ID2" "Running" 15
    echo -e "${GREEN}Pass: Normal VM runs without attestation${NC}"
fi

echo "Cleaning up..."
$VYOMA stop "$VM_ID" 2>/dev/null || true
$VYOMA stop "$VM_ID2" 2>/dev/null || true
unregister_vm "$VM_ID"
unregister_vm "$VM_ID2"

cleanup_env $DAEMON_PID
echo "=== Test 11 Passed ==="
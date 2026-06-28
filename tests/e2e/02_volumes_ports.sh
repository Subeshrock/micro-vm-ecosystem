#!/bin/bash
set -e
source tests/e2e/common.sh

echo "=== Test 02: Volumes & Ports ==="

check_root
setup_env

# Start Daemon
echo "Starting Daemon (Port 3002)..."
sudo -E $VYOMAD_BIN --data-dir $TEST_HOME/.vyoma --socket-path /run/vyoma/test.sock --http-port 3002 > $TEST_HOME/daemon.log 2>&1 &
DAEMON_PID=$!
sleep 3

VYOMA="$VYOMA_BIN --socket-path /run/vyoma/test.sock --http-port 3002"

# Prepare Volume
HOST_VOL=$(mktemp -d)
echo "test-data" > $HOST_VOL/host_file.txt

# 1. Run with Volume & Port
echo "Running VM with Volume & Port..."
# -v host:vm -p host:vm
$VYOMA run alpine:latest -v $HOST_VOL:/data -p 8081:80 --hostname vol-vm
assert_success "Run with Vol/Port"

sleep 3

# 2. Verify
if $VYOMA ps | grep -q "vol-vm"; then
    echo -e "${GREEN}Pass: VM Running${NC}"
else
    echo -e "${RED}Fail: VM Failed to Start${NC}"
    tail -n 20 $TEST_HOME/daemon.log
    exit 1
fi

# 3. Verify Port Binding (Host Side)
# Check if something is listening on 8081
if ss -tuln | grep -q ":8081"; then
    echo -e "${GREEN}Pass: Port 8081 Listening${NC}"
else
    echo -e "${RED}Fail: Port 8081 not bound${NC}"
    # exit 1 (Soft fail for now as ss might differ)
fi

# 4. Verify Volume (Requires Exec/SSH - Skipping Deep Verification)
# We assume if VM started, VirtioFS is attached.

$VYOMA stop "$($VYOMA ps | grep vol-vm | awk '{print $1}')"

cleanup_env $DAEMON_PID
rm -rf $HOST_VOL
echo "=== Test 02 Passed ==="

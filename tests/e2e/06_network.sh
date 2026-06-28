#!/bin/bash
set -e
source tests/e2e/common.sh

echo "=== Test 06: Networking ==="

check_root
setup_env

# Start Daemon
sudo -E $VYOMAD_BIN --data-dir $TEST_HOME/.vyoma --socket-path /run/vyoma/test.sock --http-port 3007 > $TEST_HOME/daemon.log 2>&1 &
DAEMON_PID=$!
sleep 3
VYOMA="$VYOMA_BIN --socket-path /run/vyoma/test.sock --http-port 3007"

# 1. List (Default)
$VYOMA network ls
assert_success "List Networks"

# 2. Create
echo "Creating Network..."
$VYOMA network create test-net --subnet 10.99.0.0/16
assert_success "Create Network"

# 3. Verify
if $VYOMA network ls | grep -q "test-net"; then
    echo -e "${GREEN}Pass: Network Found${NC}"
else
    echo -e "${RED}Fail: Network missing${NC}"
    exit 1
fi

# 4. Remove
echo "Removing Network..."
$VYOMA network rm test-net
assert_success "Remove Network"

if $VYOMA network ls | grep -q "test-net"; then
    echo -e "${RED}Fail: Network still exists${NC}"
else
    echo -e "${GREEN}Pass: Network Removed${NC}"
fi

cleanup_env $DAEMON_PID
echo "=== Test 06 Passed ==="

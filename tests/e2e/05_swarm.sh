#!/bin/bash
set -e
source tests/e2e/common.sh

echo "=== Test 05: Swarm Multi-Node ==="

check_root

# Setup Node 1
echo "Setting up Node 1..."
export TEST_HOME=$(mktemp -d)
HOME1=$TEST_HOME/node1
mkdir -p $HOME1
sudo -E HOME=$HOME1 $VYOMAD_BIN --data-dir $HOME1/.vyoma --socket-path /run/vyoma/node1.sock --http-port 3005 > $HOME1/daemon.log 2>&1 &
PID1=$!

# Setup Node 2
echo "Setting up Node 2..."
HOME2=$TEST_HOME/node2
mkdir -p $HOME2
sudo -E HOME=$HOME2 VYOMA_DAEMON_PORT=3006 $VYOMAD_BIN --data-dir $HOME2/.vyoma --socket-path /run/vyoma/node2.sock --http-port 3006 > $HOME2/daemon.log 2>&1 &
PID2=$!

sleep 3

VYOMA1="$VYOMA_BIN --socket-path /run/vyoma/node1.sock --http-port 3005"
VYOMA2="$VYOMA_BIN --socket-path /run/vyoma/node2.sock --http-port 3006"

# 1. Init Swarm
echo "Initializing Swarm on Node 1..."
$VYOMA1 swarm init
assert_success "Swarm Init"

# 2. Join Swarm
echo "Joining Node 2 to Node 1..."
# Syntax: vyoma swarm join <IP>
IGNITE_DAEMON_PORT=3005 $VYOMA2 swarm join 127.0.0.1
assert_success "Swarm Join"

# 3. List Nodes
echo "Listing Nodes..."
NODES=$($VYOMA1 swarm ls)
echo "$NODES"
if echo "$NODES" | grep -q "machine_id"; then
     echo -e "${GREEN}Pass: Nodes listed${NC}"
else
     echo -e "${RED}Fail: No nodes found${NC}"
fi

# Cleanup
echo "Cleaning up..."
kill $PID1 $PID2 || true
wait $PID1 $PID2 || true
rm -rf $HOME1 $HOME2
echo "=== Test 05 Passed ==="

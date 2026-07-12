#!/bin/bash
set -e
source tests/e2e/common.sh
check_root
setup_env
echo "Starting Daemon (3003)..."
sudo -E $VYOMAD_BIN --data-dir $TEST_HOME/.vyoma --socket-path /run/vyoma/test.sock --http-port 3003 > $TEST_HOME/daemon.log 2>&1 &
DAEMON_PID=$!
sleep 3
VYOMA="$VYOMA_BIN --socket-path /run/vyoma/test.sock --http-port 3003"
CTX=$TEST_HOME/build_ctx
mkdir -p $CTX
cat <<INNER_EOF > $CTX/Vyomafile
FROM alpine:latest
RUN echo "Vyoma Build Test" > /build_test.txt
CMD ["sleep", "60"]
INNER_EOF
$VYOMA pull alpine:latest
echo "Building Image..."
$VYOMA build $CTX 2>&1 || true
cat $TEST_HOME/daemon.log
kill $DAEMON_PID

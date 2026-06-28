#!/bin/bash

CLEANUP_SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/scripts/cleanup-all.sh"

pre_run_cleanup() {
    if [ -f "$CLEANUP_SCRIPT" ]; then
        echo "Running pre-test cleanup to ensure clean state..."
        sudo bash "$CLEANUP_SCRIPT" || true
        echo "Pre-test cleanup completed"
    else
        echo "Warning: cleanup-all.sh not found at $CLEANUP_SCRIPT"
    fi
}

post_run_cleanup() {
    echo "Running post-test cleanup..."
    sudo pkill vyomad 2>/dev/null || true
    if [ -f "$CLEANUP_SCRIPT" ]; then
        sudo bash "$CLEANUP_SCRIPT" || true
    fi
    echo "Post-test cleanup completed"
}

trap post_run_cleanup EXIT

run_test() {
    local script=$1
    echo "--------------------------------------------------"
    echo "RUNNING: $script"
    echo "--------------------------------------------------"
    if sudo -E $script; then
        echo "✅ PASS: $script"
    else
        echo "❌ FAIL: $script"
    fi
    sudo pkill vyomad 2>/dev/null || true
    echo ""
}

pre_run_cleanup

echo "Starting Full Regression Suite..."

run_test ./tests/e2e/01_lifecycle.sh
run_test ./tests/e2e/02_volumes_ports.sh
run_test ./tests/e2e/03_builder.sh
run_test ./tests/e2e/04_compose.sh
run_test ./tests/e2e/05_swarm.sh
run_test ./tests/e2e/06_network.sh
run_test ./tests/e2e/07_snapshot.sh
run_test ./tests/e2e/08_snapshot_restore.sh
run_test ./tests/e2e/09_port_forwarding.sh
run_test ./tests/e2e/10_volume_mount.sh
run_test ./tests/e2e/11_attestation.sh
run_test ./tests/e2e/11_migration.sh
run_test ./tests/e2e/12_exec.sh
run_test ./tests/e2e/13_stats.sh
run_test ./tests/e2e/14_volumes.sh

echo "Suite Completed."

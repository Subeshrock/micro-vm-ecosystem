#!/bin/bash
# Common utilities for E2E tests

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

LOG_DIR="/tmp/vyoma-tests-$(date +%s)"
mkdir -p $LOG_DIR

export VYOMAD_BIN="$(pwd)/target/release/vyomad"
export VYOMA_BIN="$(pwd)/target/release/vyoma"
export REAL_HOME=$HOME

RUNNING_VMS=()

if [ ! -f "$VYOMAD_BIN" ]; then
    echo "Error: Binary not found at $VYOMAD_BIN. Run 'make all' first."
    exit 1
fi

check_root() {
    if [ "$EUID" -ne 0 ]; then
        echo -e "${RED}Error: Tests must be run as root (for Firecracker/CNI).${NC}"
        sudo -n true 2>/dev/null || { echo "Please run with sudo or provide password."; exit 1; }
    fi
}

setup_env() {
    export TEST_HOME=$(mktemp -d)
    export HOME=$TEST_HOME
    export PATH="$(pwd)/target/release:$PATH"
    echo "Test Environment: $TEST_HOME"

    # Ensure vyoma0 bridge exists for networking
    sudo ip link add vyoma0 type bridge 2>/dev/null || true
    sudo ip addr add 172.16.0.1/24 dev vyoma0 2>/dev/null || true
    sudo ip link set vyoma0 up
    
    sudo mkdir -p /run/vyoma
    sudo chmod 0777 /run/vyoma
    sudo rm -f /run/vyoma/test.sock

    # Ensure vyoma user exists for TAP device creation
    id -u vyoma &>/dev/null || sudo useradd -r -s /bin/false vyoma
    sudo usermod -aG disk vyoma || true
    sudo usermod -aG kvm vyoma || true
    sudo usermod -aG disk,kvm vyoma

    mkdir -p $TEST_HOME/.vyoma/bin
    if [ -f "$(pwd)/cloud-hypervisor" ]; then
         cp "$(pwd)/cloud-hypervisor" $TEST_HOME/.vyoma/bin/
    elif [ -f "$(pwd)/bin/cloud-hypervisor" ]; then
         cp "$(pwd)/bin/cloud-hypervisor" $TEST_HOME/.vyoma/bin/
    elif [ -f "/var/lib/vyoma/bin/cloud-hypervisor" ]; then
         cp /var/lib/vyoma/bin/cloud-hypervisor $TEST_HOME/.vyoma/bin/
    else
         echo -e "cloud-hypervisor not found. Downloading..."
         wget -q -O $TEST_HOME/.vyoma/bin/cloud-hypervisor https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v41.0/cloud-hypervisor
         chmod +x $TEST_HOME/.vyoma/bin/cloud-hypervisor
    fi

    if [ -f "$(pwd)/kernel.bzimage" ]; then
         cp "$(pwd)/kernel.bzimage" $TEST_HOME/.vyoma/bin/vmlinux
    elif [ -f "$(pwd)/bin/vmlinux" ]; then
         cp "$(pwd)/bin/vmlinux" $TEST_HOME/.vyoma/bin/vmlinux
    elif [ -f "/var/lib/vyoma/bin/vmlinux" ]; then
         cp /var/lib/vyoma/bin/vmlinux $TEST_HOME/.vyoma/bin/
    else
         echo -e "kernel not found. Downloading..."
         wget -q -O $TEST_HOME/.vyoma/bin/vmlinux https://github.com/cloud-hypervisor/linux/releases/download/ch-release-v6.16.9-20260508/bzImage-x86_64
         chmod 644 $TEST_HOME/.vyoma/bin/vmlinux
    fi

    mkdir -p $TEST_HOME/.vyoma/cni/bin
    if [ -d "$REAL_HOME/.vyoma/cni/bin" ] && [ "$(ls -A $REAL_HOME/.vyoma/cni/bin)" ]; then
         echo "Copying CNI plugins from $REAL_HOME..."
         cp $REAL_HOME/.vyoma/cni/bin/* $TEST_HOME/.vyoma/cni/bin/
    fi

    if [ -d "/usr/lib/cni" ]; then
        cp /usr/lib/cni/* $TEST_HOME/.vyoma/cni/bin/
    elif [ -d "/opt/cni/bin" ]; then
        cp /opt/cni/bin/* $TEST_HOME/.vyoma/cni/bin/
    else
        echo -e "${RED}CNI Plugins not found. Downloading...${NC}"
        curl -sL https://github.com/containernetworking/plugins/releases/download/v1.3.0/cni-plugins-linux-amd64-v1.3.0.tgz | tar -xz -C $TEST_HOME/.vyoma/cni/bin
    fi
    if [ -f "$(pwd)/target/x86_64-unknown-linux-musl/release/vyoma-agent-vm" ]; then
         cp "$(pwd)/target/x86_64-unknown-linux-musl/release/vyoma-agent-vm" $TEST_HOME/.vyoma/bin/vyoma-agent-vm
    elif [ -f "$(pwd)/target/release/vyoma-agent-vm" ]; then
         cp "$(pwd)/target/release/vyoma-agent-vm" $TEST_HOME/.vyoma/bin/vyoma-agent-vm
    fi

    chmod -R 777 $TEST_HOME

    ls -la $TEST_HOME/.vyoma/bin/

    sudo mkdir -p /run/vyoma
    sudo chown root:vyoma /run/vyoma
    sudo chmod 0775 /run/vyoma

    # Setup /dev/mapper/control
    sudo modprobe dm_mod || true
    # Run a dummy dmsetup command to ensure /dev/mapper/control is created by udev
    sudo dmsetup version >/dev/null 2>&1 || true
    
    if [ -e "/dev/mapper/control" ]; then
        sudo chown root:disk /dev/mapper/control || true
        sudo chmod 0660 /dev/mapper/control || true
    else
        # If it somehow doesn't exist, try to create it manually
        sudo mknod /dev/mapper/control c 10 236 || true
        sudo chown root:disk /dev/mapper/control || true
        sudo chmod 0660 /dev/mapper/control || true
    fi

    # Setup cgroup for vyoma user
    if [ -d "/sys/fs/cgroup" ]; then
        sudo mkdir -p /sys/fs/cgroup/vyoma.slice
        sudo chown -R vyoma:vyoma /sys/fs/cgroup/vyoma.slice
        # Enable controllers if available
        local avail=$(cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null || echo "")
        local to_enable=""
        [[ "$avail" == *"cpu"* ]] && to_enable="+cpu "
        [[ "$avail" == *"memory"* ]] && to_enable="+memory "
        [[ "$avail" == *"io"* ]] && to_enable="+io "
        if [ -n "$to_enable" ]; then
            sudo sh -c "echo '$to_enable' > /sys/fs/cgroup/cgroup.subtree_control" || true
        fi
    fi
}

cleanup_resources() {
    echo -e "${YELLOW}Cleaning up test resources...${NC}"

    for vm_id in "${RUNNING_VMS[@]}"; do
        if [ -n "$vm_id" ]; then
            echo "Stopping VM: $vm_id"
            $VYOMA_BIN --socket-path /run/vyoma/test.sock stop "$vm_id" 2>/dev/null || true
        fi
    done

    pkill -f "vyomad.*test.sock" 2>/dev/null || true
    sleep 1

    sudo dmsetup remove_all 2>/dev/null || true
    losetup -D 2>/dev/null || true

    rm -rf $TEST_HOME 2>/dev/null || true
    rm -rf /tmp/vyoma-tests-* 2>/dev/null || true
}

cleanup_env() {
    set +e
    local pid=$1
    echo "Dumping serial logs from $TEST_HOME in cleanup_env:"
    for log in $TEST_HOME/.vyoma/vms/*/serial.log; do
        if [ -f "$log" ]; then
            echo "--- $log ---"
            cat "$log"
        fi
    done
    
    echo "Dumping daemon.log:"
    if [ -f "$TEST_HOME/daemon.log" ]; then
        cat "$TEST_HOME/daemon.log"
    fi

    if [ -n "$pid" ]; then
        kill $pid 2>/dev/null || true
        wait $pid 2>/dev/null || true
    fi
    # Setup /dev/mapper/control
    sudo modprobe dm_mod || true
    # Run a dummy dmsetup command to ensure /dev/mapper/control is created by udev
    sudo dmsetup version >/dev/null 2>&1 || true
    
    if [ -e "/dev/mapper/control" ]; then
        sudo chown root:disk /dev/mapper/control || true
        sudo chmod 0660 /dev/mapper/control || true
    else
        # If it somehow doesn't exist, try to create it manually
        sudo mknod /dev/mapper/control c 10 236 || true
        sudo chown root:disk /dev/mapper/control || true
        sudo chmod 0660 /dev/mapper/control || true
    fi
    pkill -P $$ vyomad 2>/dev/null || true
    sudo dmsetup remove_all 2>/dev/null || true
    losetup -D 2>/dev/null || true
    # rm -rf $TEST_HOME 2>/dev/null || true
}

# trap cleanup_resources EXIT

handle_error() {
    echo -e "${RED}Test Error - Cleaning up...${NC}"
    echo "Dumping serial logs from $TEST_HOME:"
    for f in $TEST_HOME/.vyoma/vms/*/serial.log; do
        if [ -f "$f" ]; then
            echo "--- $f ---"
            cat "$f"
        fi
    done
    pkill vyomad 2>/dev/null || true
    # rm -rf /tmp/vyoma-tests-* 2>/dev/null || true
}

assert_success() {
    if [ $? -ne 0 ]; then
        echo -e "${RED}Test Failed: $1${NC}"
        exit 1
    else
        echo -e "${GREEN}Pass: $1${NC}"
    fi
}

wait_for_vm_state() {
    local vm_id=$1
    local expected_state=$2
    local timeout=${3:-30}
    local interval=1

    local elapsed=0
    while [ $elapsed -lt $timeout ]; do
        # Extract status using curl on the daemon socket
        local current_state=$(sudo curl -s --unix-socket /run/vyoma/test.sock http://localhost/ps | grep -o "\"id\":\"$vm_id\"[^\}]*" | grep -o "\"status\":\"[^\"]*\"" | cut -d'"' -f4)
        
        if [ "$current_state" == "$expected_state" ]; then
            echo "VM $vm_id reached state: $expected_state"
            return 0
        fi

        sleep $interval
        elapsed=$((elapsed + interval))
    done

    echo -e "${RED}Timeout: VM $vm_id did not reach state $expected_state within ${timeout}s${NC}"
    return 1
}

wait_for_port() {
    local port=$1
    local timeout=${2:-30}
    local interval=1

    local elapsed=0
    while [ $elapsed -lt $timeout ]; do
        if ss -tln 2>/dev/null | grep -q ":$port " || ss -tln 2>/dev/null | grep -q "0.0.0.0:$port"; then
            echo "Port $port is now listening"
            return 0
        fi

        if curl -s -o /dev/null -w "%{http_code}" "http://localhost:$port" 2>/dev/null | grep -q "200\|301\|302"; then
            echo "Port $port is responding"
            return 0
        fi

        sleep $interval
        elapsed=$((elapsed + interval))
    done

    echo -e "${RED}Timeout: Port $port not available within ${timeout}s${NC}"
    return 1
}

vyoma_run_and_get_id() {
    local extra_args="$@"
    local output=$($VYOMA_BIN --socket-path /run/vyoma/test.sock run $extra_args 2>&1)

    local vm_id=$(echo "$output" | grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' | tail -1 | tr -d '[:space:]')
    if [ -z "$vm_id" ]; then
        echo "vyoma run failed! Output: $output" >&2
        vm_id=$($VYOMA_BIN --socket-path /run/vyoma/test.sock ps 2>/dev/null | grep -E "$extra_args" | head -1 | awk '{print $1}')
    fi

    echo "$vm_id"
}

register_vm() {
    RUNNING_VMS+=("$1")
}

unregister_vm() {
    local vm_id=$1
    local new_array=()
    for v in "${RUNNING_VMS[@]}"; do
        [ "$v" != "$vm_id" ] && new_array+=("$v")
    done
    RUNNING_VMS=("${new_array[@]}")
}

wait_for_vm_state_from_cli() {
    local vyoma_cmd="$1"
    local vm_id=$2
    local expected_state=$3
    local timeout=${4:-30}
    local interval=1

    local elapsed=0
    while [ $elapsed -lt $timeout ]; do
        local current_state=$($vyoma_cmd ps 2>/dev/null | grep "$vm_id" | awk '{print $NF}' | tr -d '[]')

        if [ "$current_state" = "$expected_state" ]; then
            echo "VM $vm_id reached state: $expected_state"
            return 0
        fi

        sleep $interval
        elapsed=$((elapsed + interval))
    done

    echo -e "${RED}Timeout: VM $vm_id did not reach state $expected_state within ${timeout}s${NC}"
    return 1
}

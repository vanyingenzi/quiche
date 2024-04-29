#!/bin/bash

RUST_PLATFORM="x86_64-unknown-linux-gnu"
FILE_SIZE=2
NB_RUNS=10

RED='\033[0;31m'
RESET='\033[0m'

ROOT_DIR="$(pwd)/www"
FILESIZE_BYTES=$((FILE_SIZE * 1024 * 1024 * 1024))
MCPQUIC_AVERAGE_EXEC_TIME=100000000000 # MAX

echo_red() {
    echo -e "${RED}$1${RESET}" >&2
}

get_unused_port(){
    local port
    port=$(shuf -i 2000-65000 -n 1)
    while netstat -atn | grep -q ":$port "; do
        port=$(shuf -i 2000-65000 -n 1)
    done
    echo "$port"
}

setup_rust() {
    # Rust
    if ! rustc --version 1>/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -sSf -o /tmp/rustup-init.sh https://sh.rustup.rs
        chmod +x /tmp/rustup-init.sh
        /tmp/rustup-init.sh -q -y --default-host "$RUST_PLATFORM" --default-toolchain stable --profile default
        source "$HOME/.cargo/env"
    else 
        echo "Rust is already installed"
    fi
}

mcmpquic_iteration_loop() {
    RUSTFLAGS='-C target-cpu=native' cargo build --release || exit 1
    local server_pid server_port client_port_1 client_port_2 error_code
    local total_runtime=0
    for iter in $(seq 1 ${NB_RUNS}); do
        echo "Benchmarking multicore Multi-Path QUIC [mcMPQUIC] - Iteration $iter"
        client_port_1=$(get_unused_port)
        client_port_2=$(get_unused_port)

        # Run server
        sudo pkill quiche-server 1>/dev/null 2>&1
        sudo fuser -k 4433/udp 1>/dev/null 2>&1
        sudo fuser -k 3344/udp 1>/dev/null 2>&1
        sudo fuser -k ${client_port_1}/udp 1>/dev/null 2>&1
        sudo fuser -k ${client_port_2}/udp 1>/dev/null 2>&1

        ../target/release/quiche-server \
            --key "$(pwd)/src/bin/cert.key" \
            --cert "$(pwd)/src/bin/cert.crt" \
            --listen 127.0.0.1:4433 \
            --server-address 127.0.0.2:3344 \
            --transfer-size ${FILESIZE_BYTES} \
            --multipath --multicore 1>/dev/null 2>&1 &
        server_pid=$!

        # Run client
        start=$(date +%s.%N)
        ../target/release/quiche-client \
            -A 127.0.0.1:${client_port_1} \
            -A 127.0.0.1:${client_port_2} \
            --connect-to 127.0.0.1:4433 \
            --server-address 127.0.0.2:3344 \
            --multipath --multicore --no-verify --wire-version 1 1>/dev/null 2>&1
        error_code=$?
        end=$(date +%s.%N)
        if [ $error_code -ne 0 ]; then
            echo_red "Error Client: $error_code"
            exit $error_code
        fi
        runtime=$(echo "$end - $start" | bc)
        total_runtime=$(echo "$total_runtime + $runtime" | bc)
        kill ${server_pid} 1>/dev/null 2>&1
    done
    average_runtime=$(echo "scale=9; $total_runtime / $NB_RUNS" | bc)
    MCPQUIC_AVERAGE_EXEC_TIME=$average_runtime
    echo "Average Runtime: $average_runtime seconds" >&2
}

mpquic_iteration_loop() {
    RUSTFLAGS='-C target-cpu=native' cargo build --release || exit 1
    local server_pid server_port client_port_1 client_port_2 error_code
    local total_runtime=0
    for iter in $(seq 1 ${NB_RUNS}); do
        echo "Benchmarking Multi-Path QUIC [MPQUIC] - Iteration $iter" >&2
        client_port_1=$(get_unused_port)
        client_port_2=$(get_unused_port)

        # Run server
        sudo pkill quiche-server 1>/dev/null 2>&1
        sudo fuser -k 4433/udp 1>/dev/null 2>&1
        sudo fuser -k 3344/udp 1>/dev/null 2>&1
        sudo fuser -k ${client_port_1}/udp 1>/dev/null 2>&1
        sudo fuser -k ${client_port_2}/udp 1>/dev/null 2>&1
                
        ../target/release/quiche-server \
            --key "$(pwd)/src/bin/cert.key" \
            --cert "$(pwd)/src/bin/cert.crt" \
            --listen 127.0.0.1:4433 \
            --server-address 127.0.0.2:3344 \
            --transfer-size ${FILESIZE_BYTES} \
            --multipath 1>/dev/null 2>&1 &
        server_pid=$!

        # Run client
        start=$(date +%s.%N)
        ../target/release/quiche-client \
            -A 127.0.0.1:${client_port_1} \
            -A 127.0.0.1:${client_port_2} \
            --connect-to 127.0.0.1:4433 \
            --server-address 127.0.0.2:3344 \
            --multipath --no-verify --wire-version 1 1>/dev/null 2>&1
        error_code=$?
        end=$(date +%s.%N)
        if [ $error_code -ne 0 ]; then
            echo_red "Error Client: $error_code"
            exit $error_code
        fi
        runtime=$(echo "$end - $start" | bc)
        total_runtime=$(echo "$total_runtime + $runtime" | bc)
        kill ${server_pid} 1>/dev/null 2>&1
    done
    average_runtime=$(echo "scale=9; $total_runtime / $NB_RUNS" | bc)
    echo "Average Runtime: $average_runtime seconds" >&2
    if (( $(echo "$average_runtime < $MCPQUIC_AVERAGE_EXEC_TIME" | bc -l) )); then
        echo "MPQUIC is faster than mcMPQUIC" >&2
        return 1
    else
        echo "mcMPQUIC is faster than MPQUIC" >&2
        return 0
    fi
}

# Similar modifications should be made to mpquic_iteration_loop

main() {
    # Version
    setup_rust
    [ $? -ne 0 ] && { echo_red "Error setting up rust"; exit 1; }
    mcmpquic_iteration_loop
    [ $? -ne 0 ] && { echo_red "Error running mcMPQUIC iteration loop"; exit 1; }
    return_code=$(mpquic_iteration_loop)
    exit $return_code
}

main
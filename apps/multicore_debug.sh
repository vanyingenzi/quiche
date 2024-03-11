#!/bin/bash

# Code partlty inspired by https://github.com/tumi8/quic-10g-paper

# Variables

#Initially "https://github.com/qdeconinck/quiche.git"
#Vany's "https://github.com/vanyingenzi/quiche.git"
QUICHE_REPO="https://github.com/cloudflare/quiche.git"
#Initially "d87332018d84fb7c429ad2ed34cbfdc6ee9477c8"
#Vany's "739ed2d8bafe963815134bce4ffffe8258c51e83"
QUICHE_COMMIT="4dfe0b906d08460882c9b29cd660015c571f024e"
RUST_PLATFORM="x86_64-unknown-linux-gnu"
FILE_SIZE=1G
NB_RUNS=1

RED='\033[0;31m'
RESET='\033[0m'

ROOT_DIR="$(pwd)/www"
LOGS_DIR="$(pwd)/correctness_test_logs"
RESPONSES_DIR="$(pwd)/correctness_test_responses"

FILENAME="${FILE_SIZE}B_file"
echo_red() {
    echo -e "${RED}$1${RESET}"
}

get_unused_port(){
    local port
    port=$(shuf -i 2000-65000 -n 1)
    while netstat -atn | grep -q ":$port "; do
        port=$(shuf -i 2000-65000 -n 1)
    done
    echo "$port"
}

clone_mp_quiche() {
    if [ ! -d "$(pwd)/quiche" ]; then
        git clone --recursive "$QUICHE_REPO"
        cd quiche || exit 1
        git checkout "$QUICHE_COMMIT"
        cd ..
    fi
    cd quiche || exit 1
    RUSTFLAGS='-C target-cpu=native' cargo build --release || exit 1
    cd ..
    cp "quiche/target/release/quiche-client" $(pwd) || exit 1
    cp "quiche/target/release/quiche-server" $(pwd) || exit 1
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

setup_environment() {
    mkdir -p "${ROOT_DIR}" "${RESPONSES_DIR}" "${LOGS_DIR}"
    fallocate -l ${FILE_SIZE} "${ROOT_DIR}/${FILENAME}"
}

iteration_loop() {
    local server_pid server_port client_port_1 client_port_2 tcpdump_pid error_code
    for iter in $(seq 1 ${NB_RUNS}); do
        echo "Testing Multi-Path QUIC correctness - Iteration $iter"
        client_port_1=$(get_unused_port)
        client_port_2=$(get_unused_port)

        # tcpdump -i any -s 96 -U "udp and host 127.0.0.1 and (port ${server_port} or port ${client_port_1} or port ${client_port_2}) and ip" -w "${LOGS_DIR}/${iter}.pcap" 2>/dev/null &
        # tcpdump_pid=$!
        # [ $? -ne 0 ] && { echo_red "Error tcpdump"; exit 1; }

        # sleep 10 # Give tcpdump some time
        
        # Run server
        ../target/release/quiche-server \
            --listen 127.0.0.1:${server_port} \
            --root "${ROOT_DIR}" \
            --key "$(pwd)/quiche/apps/src/bin/cert.key" \
            --cert "$(pwd)/quiche/apps/src/bin/cert.crt" \
            --multipath \
        server_pid=$!

        # Run client
        ../target/release/quiche-client \
            --no-verify "https://127.0.0.1:${server_port}/${FILENAME}" \
            --dump-responses "${RESPONSES_DIR}"
        error_code=$?

        # sleep 10 # Give tcpdump some time
        
        sudo kill -9 ${server_pid} 1>/dev/null 2>&1
        if [ $error_code -ne 0 ]; then
            echo_red "Error Client: $error_code"
            exit 1
        fi

        # kill -9 ${tcpdump_pid} 1>/dev/null 2>&1

        # Check if files are the same
        diff -q "${ROOT_DIR}/${FILENAME}" "${RESPONSES_DIR}/${FILENAME}"
        if [ $? -ne 0 ]; then
            echo_red "Error: files are not the same"
            exit 1
        fi
    done

    sudo chmod 777 ${LOGS_DIR}/*
}

main() {
    # Version
    setup_rust
    [ $? -ne 0 ] && { echo_red "Error setting up rust"; exit 1; }
    clone_mp_quiche
    [ $? -ne 0 ] && { echo_red "Error cloning quiche"; exit 1; }
    setup_environment
    [ $? -ne 0 ] && { echo_red "Error setting up environment"; exit 1; }
    iteration_loop
}

main

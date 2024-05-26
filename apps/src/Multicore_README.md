# Multicore Impl

```bash
RUSTFLAGS='-C target-cpu=native' cargo build --release
mkdir -p /tmp/www; fallocate -l 1G /tmp/www/testfile
```

server
```bash
RUST_LOG=info ../target/release/quiche-server --listen 127.0.0.1:4433 --multipath --multicore --transfer-size 1073741824 --server-address 127.0.0.2:3344
```

client
```bash
RUST_LOG=info ../target/release/quiche-client --no-verify -A 127.0.0.1:6788 -A 127.0.0.1:6789 --connect-to 127.0.0.1:4433 --server-address 127.0.0.2:3344 --multipath --multicore --wire-version 1 >/dev/null
```

```bash
sudo perf record -e cycles -F 999 -g --call-graph lbr -- ../target/release/quiche-client --no-verify -A 127.0.0.1:6788 -A 127.0.0.1:6789 --connect-to 127.0.0.1:4433 --server-address 127.0.0.2:3344 --multipath --multicore --wire-version 1 >/dev/null
```

```bash
cargo test multicore
```
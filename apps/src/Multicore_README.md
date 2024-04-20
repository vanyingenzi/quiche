# Multicore Impl

```bash
RUSTFLAGS='-C target-cpu=native' cargo build --release
mkdir -p /tmp/www; fallocate -l 1G /tmp/www/testfile
```

server
```bash
RUST_LOG=info ../target/release/quiche-server --root /tmp/www --multipath --multicore
```

client 
```bash
RUST_LOG=info ../target/release/quiche-client https:127.0.0.1:4433/testfile --no-verify -A 127.0.0.1:6788 -A 127.0.0.1:6789 --multipath --multicore --wire-version 1 >/dev/null
```

```bash
sudo perf record -e cycles -F 999 -g --call-graph lbr -- ../target/release/quiche-client https:127.0.0.1:4433/testfile --no-verify -A 127.0.0.1:6788 -A 127.0.0.1:6789 --multipath --multicore >/dev/null
```

```bash
cargo test --features=multicore
```
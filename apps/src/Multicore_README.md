# Multicore Impl

```bash
cargo build --release
mkdir -p /tmp/www; fallocate -l 4G /tmp/www/testfile
```

server
```bash
RUST_LOG=info ../target/release/quiche-server --root /tmp/www --multipath --multicore
```

client 
```bash
RUST_LOG=info ../target/release/quiche-client https:127.0.0.1:4433/testfile --no-verify -A 127.0.0.1:6788 -A 127.0.0.1:6789 --multipath --multicore 1>/dev/null
```

```bash
sudo perf record -F1000 -ag -- ../target/release/quiche-client https:127.0.0.1:4433/testfile --no-verify -A 127.0.0.1:6788 -A 127.0.0.1:6789 --multipath 1>/dev/null
```
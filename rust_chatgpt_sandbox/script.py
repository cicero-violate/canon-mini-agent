bash -lc set -euo pipefail
mkdir -p /mnt/data/canon-mini-agent-extracted /mnt/data/rust-sandbox /mnt/data/.cargo
rm -rf /mnt/data/canon-mini-agent-extracted/*
tar -xzf /mnt/data/canon-mini-agent.tar.gz -C /mnt/data/canon-mini-agent-extracted
RT=/mnt/data/rust-nightly-x86_64-unknown-linux-gnu
if [ ! -d "$RT" ]; then tar -xzf /mnt/data/rust-nightly-x86_64-unknown-linux-gnu.tar.gz -C /mnt/data; fi
rm -rf /mnt/data/rust-sandbox/*
mkdir -p /mnt/data/rust-sandbox/bin /mnt/data/rust-sandbox/lib/rustlib/x86_64-unknown-linux-gnu/lib /mnt/data/rust-sandbox/lib/rustlib/x86_64-unknown-linux-gnu/bin
cp -a "$RT/rustc/bin"/* /mnt/data/rust-sandbox/bin/
cp -a "$RT/cargo/bin"/* /mnt/data/rust-sandbox/bin/
cp -a "$RT/rustc/lib" /mnt/data/rust-sandbox/
cp -a "$RT/rust-std-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-gnu/lib"/* /mnt/data/rust-sandbox/lib/rustlib/x86_64-unknown-linux-gnu/lib/
if [ -d "$RT/rust-std-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-gnu/bin" ]; then cp -a "$RT/rust-std-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-gnu/bin"/* /mnt/data/rust-sandbox/lib/rustlib/x86_64-unknown-linux-gnu/bin/; fi
cat > /mnt/data/.cargo/config.toml <<EOF
[registries.artifactory]
index = "sparse+https://${CAAS_ARTIFACTORY_CARGO_REGISTRY}/"
credential-provider = "cargo:token"
[source.crates-io]
replace-with = "artifactory"
[source.artifactory]
registry = "sparse+https://${CAAS_ARTIFACTORY_CARGO_REGISTRY}/"
EOF
cat > /mnt/data/.cargo/credentials.toml <<EOF
[registries.artifactory]
token = "Basic $(printf '%s:%s' "$CAAS_ARTIFACTORY_READER_USERNAME" "$CAAS_ARTIFACTORY_READER_PASSWORD" | base64 -w0)"
EOF
mkdir -p /mnt/data/canon-mini-agent-extracted/canon
cat > /mnt/data/canon-mini-agent-extracted/canon/Cargo.toml <<'EOF'
[workspace]
members = ["canon-utils/canon-tools-patch"]
resolver = "2"
[workspace.dependencies]
anyhow = "1"
thiserror = "1"
EOF
cd /mnt/data/canon-mini-agent-extracted/canon-mini-agent
PATH=/mnt/data/rust-sandbox/bin:/usr/bin:/bin CARGO_HOME=/mnt/data/.cargo RUSTC=/mnt/data/rust-sandbox/bin/rustc /mnt/data/rust-sandbox/bin/cargo build -q > /tmp/cma_build.log 2>&1; echo STATUS:$?

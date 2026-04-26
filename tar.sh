# tar -czf canon-mini-agent.tar.gz -C /workspace/ai_sandbox \
# canon-mini-agent/.git \
# canon-mini-agent/.gitignore \
# canon-mini-agent/canon-chromium-extension \
# canon-mini-agent/state/rustc \
# canon-mini-agent/tests \
# canon-mini-agent/plan \
# canon-mini-agent/INVARIANTS.json \
# canon-mini-agent/Cargo.toml \
# canon-mini-agent/src \
# canon-mini-agent/SPEC.md \
# canon-mini-agent/agent_state \
# canon-mini-agent/frames \
# canon-mini-agent/AUTHORITY_MATRIX.md \
# canon-mini-agent/state \
# canon-rustc-v2/Cargo.toml \
# canon-rustc-v2/Cargo.lock \
# canon-rustc-v2/rust-toolchain.toml \
# canon-rustc-v2/src

git add .
git commit -m "uploading to chatgpt projects"
git push origin main
tar -czf canon-mini-agent.tar.gz -C /workspace/ai_sandbox \
canon-mini-agent/.git canon-mini-agent/.gitignore canon-mini-agent/.cargo \
canon-mini-agent/Cargo.toml canon-mini-agent/Cargo.lock \
canon-mini-agent/src canon-mini-agent/tests canon-mini-agent/plan \
canon-mini-agent/SPEC.md canon-mini-agent/INVARIANTS.json canon-mini-agent/AUTHORITY_MATRIX.md \
canon-mini-agent/rubric \
canon-mini-agent/canon-chromium-extension canon-mini-agent/state canon-mini-agent/agent_state canon-mini-agent/frames \
canon-rustc-v2/Cargo.toml canon-rustc-v2/Cargo.lock canon-rustc-v2/rust-toolchain.toml canon-rustc-v2/src

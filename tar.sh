tar -czf canon-mini-agent.tar.gz -C /workspace/ai_sandbox \
canon-mini-agent/state/rustc \
canon-mini-agent/tests \
canon-mini-agent/INVARIANTS.json \
canon-mini-agent/Cargo.toml \
canon-mini-agent/PLAN.json \
canon-mini-agent/src \
canon-mini-agent/SPEC.md \
canon-mini-agent/ISSUES.json
# canon-mini-agent/agent_state \
# canon-mini-agent/frames 

{ on = [ "<A-n>" ], run = ''' shell 'ripdrag "$@" -nxa 2>/dev/null &' --confirm ''' },

# canon-mini-agent/target/debug/deps \
# canon-mini-agent/target/debug/.fingerprint \
# canon-mini-agent/target/debug/build \

STATE_SPACE_REPORT=1 cargo test -p canon-mini-agent --test invalid_action_harness -- --nocapture
STATE_SPACE_REPORT=1 cargo test -p canon-mini-agent state_space_tests -- --nocapture
ORCH_STATE_SPACE_REPORT=1 cargo test -p canon-mini-agent --test orchestrator_harness -- --nocapture

# Control-to-Tlog Audit

Remaining file-backed control surfaces that still influence runtime behavior:

1. `agent_state/rust_patch_verification_requested.flag`
   - producer: `src/tools.rs:78-90`
   - consumers: `src/supervisor.rs:216-225`, `src/supervisor.rs:339-369`, `src/supervisor.rs:1327-1350`
   - why it matters: controls whether supervisor runs build/test gates before restart and in loop.

2. `agent_state/orchestrator_mode.flag`
   - producer: `src/app.rs:6003-6011`, `src/app.rs:6842-6844`
   - consumer: `src/supervisor.rs:243-251`, `src/supervisor.rs:969-990`
   - why it matters: controls restart path (`orchestrate` vs single-role).

3. `agent_state/orchestrator_cycle_idle.flag`
   - producer lifecycle: `src/app.rs:6403-6405`, `src/app.rs:6837-6839`, `src/app.rs:6902`
   - consumer: `src/supervisor.rs:239-256`, `src/supervisor.rs:993-1023`
   - why it matters: mtime freshness gates deferred restart timing.

4. `agent_state/mini_agent_checkpoint.json`
   - producer: `src/app.rs:2215-2301`
   - consumer: `src/app.rs:2363-2411`, `src/app.rs:6073-6139`
   - why it matters: checkpoint contents still steer phase resume and verifier carry-over.

5. `agent_state/post_restart_result.json`
   - producer: `src/app.rs:4798-4811`, `src/app.rs:4829-4855`
   - consumers: `src/app.rs:683-687`, `src/app.rs:2343-2355`, `src/app.rs:5668-5672`, `src/app.rs:4891-4948`
   - why it matters: injects last completed action result into restarted planner/executor prompts and verifier recovery.

6. `agent_state/last_planner_blocker_evidence.txt`
   - producers/consumers: `src/tools.rs:2361-2391`, `src/app.rs:3787-3807`
   - why it matters: suppresses repeated planner blocker messages when evidence is unchanged.

Already tlog-backed / projection-only:
- `last_message_to_<role>.json` and wake routing are canonically backed by `InboundMessageQueued/Consumed` and `WakeSignalQueued/Consumed`.
- `external_user_message_to_<role>.json` is backed by `ExternalUserMessageRecorded` + `ExternalUserMessageConsumed`.
- `active_blocker_to_verifier.json` is a projection; control authority is `VerifierBlockerSet`.

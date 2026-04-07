cargo run -p canon-mini-agent --bin canon-mini-supervisor \
    --orchestrate \
    --workspace /workspace/ai_sandbox/canon-mini-agent \
    --instance agent_0 \
    --port 9103

cargo run -p canon-mini-agent --bin canon-mini-supervisor -- \
  --orchestrate --instance agent_0 --port 9103



while true; do
  /workspace/ai_sandbox/canon/target/debug/canon-mini-agent --orchestrate --instance agent_0 --port 9103
  echo "restarting..."
  sleep 1
done



while true; do
/workspace/ai_sandbox/canon/target/debug/canon-mini-agent --orchestrate --instance agent_1 --port 9104
  echo "restarting..."
  sleep 1
done

/workspace/ai_sandbox/canon/target/debug/canon-mini-agent --orchestrate --instance agent_2 --port 9105

/workspace/ai_sandbox/canon/target/debug/canon-mini-agent --orchestrate --instance agent_3 --port 9106


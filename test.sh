tail -50 agent_state/tlog.ndjson | python3 -c "
import sys, json
for line in sys.stdin:
    r = json.loads(line)
    if r.get('event', {}).get('event', {}).get('kind') == 'eval_score_recorded':
        print(json.dumps(r['event']['event'], indent=2))
"

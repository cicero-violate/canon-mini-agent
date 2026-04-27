Variables:
D=data, 
M=model, 
EVAL=eval, 
RL=reinforcement/update, 
J=judgment, 
P=plan, 
X=execute, 
V=verify, 
W=write, 
R=recovery, 
L=learn, 
G=good

Equations:
Train: M' = RL(EVAL(M, D))
Runtime: prompt → M → (J → P → X → V → W)
Feedback: EVAL(runtime) → RL → M'

Canonical SOTA Pipeline:
D → M → J → P → X → V → W → EVAL → RL → M'

Roles:
Data = scale + coverage
Model = compressed intelligence
Judgment = implicit (inside model)
Plan = implicit or shallow
Execute = tool use / code
Verify = tests / heuristics
Write = logs / artifacts
Eval = benchmarks + reward models
RL = weight update / fine-tuning
Learn = model weight change

Limits:
No explicit invariants
Weak deterministic recovery
Learning = slow (offline or batch RL)
Opaque judgment

Good:
G = max(D, M, EVAL, RL)

1-line:
SOTA systems rely on data → model → eval → reinforcement loops, with most intelligence embedded inside the model rather than explicit invariant/recovery pipelines.

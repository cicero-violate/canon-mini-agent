Variables:
G=good

E=error, 
I=invariant, 
J=judgment, 
P=plan, 
X=execute, 
V=verify, 
W=write, 
EVAL=eval, 
R=recovery, 
L=learn, 
T=tlog

Equation:
E → I → J → P → X → V → W → EVAL → R → L → J'

Roles:
Invariant = constraint / guard
Judgment = router
Plan = intended path
Execute = state mutation attempt
Verify = correctness gate
Write = source of truth
Eval = measurement
Recovery = correction
Learn = policy/invariant promotion
Error = signal that starts the loop

Rule:
W before EVAL, R, L

Good:
G = max(I, J, EVAL, R, L, W)

1-line:
Error triggers invariant check, judgment routes, plan defines, execute mutates, verify gates, write records, eval measures, recovery corrects, learning upgrades invariants and judgment.

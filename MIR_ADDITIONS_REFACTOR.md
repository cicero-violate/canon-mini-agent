  1. Monomorphization Explosion — MIR only

  A generic function instantiated N times in MIR with identical bodies across all monomorphizations is a type-erasure opportunity. The compiler is
   doing redundant work the programmer doesn't see.

  mono(s) = |{Mᵢ : Fᵢ = F₀}| where M = set of monomorphizations of s
  W = EraseToTraitObject(s)  if mono(s) > threshold

  semantic.rs already produces mir_fingerprint per symbol — extend it to group by base symbol name, collect all monomorphized variants, check
  fingerprint equality across them.

  ---
  2. Computed-But-Unread (Dead Assignment Inside Live Function)

  Different from dead code — the function is called, but some internal computation is written to a MIR place that is never subsequently read
  before function exit. The borrow checker allows this; MIR exposes it explicitly.

  dark(s) = |{p ∈ Places(s) : written(p) ∧ ¬read(p)}| / S
  W = RemoveDarkComputation(s, place)  if dark(s) > 0

  This catches things like: computing a value for logging that was removed, accumulating a counter that's never returned, building a struct field
  that's overwritten before use.

  ---
  3. Clone Pressure Score — MIR explicit clone tracking

  MIR desugars .clone() into explicit Clone::clone() call terminators. The ratio of clone calls to total statements reveals functions that are
  copying data unnecessarily.

  clone_pressure(s) = clone_terminators(s) / S
  W = RefactorToReference(s)  if clone_pressure(s) > α ∧ C_in > β

  High clone_pressure + high C_in = hot path that's copying on every call. The agent task is to identify which argument could be &T instead of T.

  ---
  4. Drop Elaboration Complexity — conditional ownership

  Rust's drop elaboration inserts explicit Drop terminators at MIR level. A function where drops appear in only some branches of the CFG has
  conditional ownership — complex borrowing that confuses both humans and the borrow checker.

  drop_complexity(s) = |{b ∈ Blocks(s) : Drop ∈ terminators(b)}| / B
  W = SimplifyConditionalDrop(s)  if drop_complexity(s) > γ ∧ B > 3

  ---
  5. Implicit State Machine Detection

  CFG pattern: multiple SwitchInt terminators + back-edges (cycles in the block graph) + a dominant entry block. This is an implicit state machine
   encoded as a function.

  state_machine(s) = 1[switchint_count(s) > τ ∧ cyclic_blocks(s) > 0]
  W = ExtractExplicitStateMachine(s)

  The work task tells the agent: "this function is a disguised state machine, extract the state enum and transition table." The proof is the CFG
  structure from graph_cfg. Build+test verifies behavior is preserved.

  ---
  6. HIR Visibility Leak — pub items with private reach

  A pub or pub(crate) item whose only reference sites are within a single module. HIR has exact visibility and ref_count per call site location.

  visibility_gap(s) = declared_visibility(s) - minimum_required_visibility(s)
  W = TightenVisibility(s, new_vis)  if visibility_gap(s) > 0

  This is zero-behavior-change — purely mechanical. High confidence, low risk, directly measurable. The agent doesn't need to reason about
  semantics at all.

  ---
  7. Panic Surface Area — Assert terminator density

  MIR Assert terminators are explicit panics (bounds checks, overflow checks, explicit assert!). High density = large "panic surface" = candidate
  for Result-based error propagation.

  panic_surface(s) = assert_terminators(s) / B
  W = PanicToResult(s)  if panic_surface(s) > δ ∧ C_in > ε

  Particularly powerful when combined with HIR: if the callers of s already handle Result, the function is the last link that should be converted.

  ---
  8. HIR Trait Implementation Orphans

  A trait impl where no call site ever dispatches through the trait — only through the concrete type. The impl exists for interface compliance but
   is never used as an interface.

  dead_impl(T, Tr) = 1[dyn Tr ∉ usage_sites(T) ∧ impl Tr ∉ where_bounds(T)]
  W = RemoveOrJustifyImpl(T, Tr)

  HIR gives you dyn Trait usage sites. If none reference the type, the impl is either dead or exists for an unstated reason — either way the agent
   should interrogate it.

  ---
  9. HIR + MIR: Generic Overreach

  A generic function fn f<T: Bound>(x: T) where every actual monomorphization in MIR uses only one concrete type. The generic parameter is doing
  nothing.

  overreach(s) = 1[|mono_types(s)| = 1 ∧ type_params(s) > 0]
  W = Concretize(s, concrete_type)

  The agent task is mechanical: replace T with the single actual type used. The proof is the MIR monomorphization set.

  ---
  10. Fingerprint Drift as GRPO Reward Signal

  This one connects everything to the GRPO discussion. Since mir_fingerprint is stable across builds, track the delta per symbol between tlog
  evolution N and N+1:

  Δfingerprint(s, t) = fingerprint(s,t) ≠ fingerprint(s,t-1)
  improved(s) = Δfingerprint(s) ∧ complexity(s,t) < complexity(s,t-1)
  regressed(s) = Δfingerprint(s) ∧ complexity(s,t) > complexity(s,t-1)

  R(episode) = Σ improved(s) - λ · Σ regressed(s)

  Every time the agent modifies code, the MIR fingerprints change. The reward signal for GRPO is automatic — no human labeling, no LLM judge. The
  compiler's own IR tells you if the change was an improvement. This closes the self-improvement loop at the MIR level.

  ---
  11. Loop Invariant Waste

  In MIR's CFG, a back-edge identifies a loop. Computations inside the loop that use only values defined outside the loop header are invariants
  that could be hoisted.

  loop_waste(s) = |{stmts ∈ loop_body(s) : uses(stmt) ⊆ loop_invariant_defs(s)}|
  W = HoistLoopInvariant(s, stmt)  if loop_waste(s) > 0

  ---
  12. Cross-Crate Cohesion Map

  For each module, compute the ratio of edges that stay within the module vs cross module boundaries using the call graph from graph_call:

  cohesion(m) = |internal_edges(m)| / (|internal_edges(m)| + |external_edges(m)|)
  W = ExtractModule(m) or MergeIntoParent(m)  based on cohesion direction

  Low cohesion = module that mostly calls other modules (should be dissolved or reorganized).
  High cohesion with high external ref_count = well-bounded module that should have a formal API surface.

  ---
  The Unified Score

  Extending your P = αsplit + βdup + γdead + δwrapper to the full signal set:

  P(s) = α·split(s) + β·dup(s) + γ·dead(s) + δ·wrapper(s)
        + ε·clone_pressure(s) + ζ·panic_surface(s) + η·drop_complexity(s)
        + θ·mono(s) + ι·dark(s) + κ·visibility_gap(s)

  Each coefficient is learnable from historical task outcomes — tasks that passed build+test with high Δimproved get their signal weights
  increased. This is the GRPO reward feeding back into the work generator. The work generator improves itself.

  ---
  Key Insight

  HIR answers: what shape is this code in structurally?
  MIR answers: what does this code actually do at runtime?
  Their intersection answers: what is structurally complex but computationally redundant? — which is exactly the highest-value refactor target.

  The signals above that are purely MIR (fingerprint, clone pressure, drop complexity, loop invariant) find runtime inefficiency. The signals that
   are purely HIR (visibility gap, dead impl, generic overreach) find structural debt. The combined signals (state machine, monomorphization
  explosion) find abstraction mismatches — places where the programmer's model and the compiler's model have diverged.

# Graph Reduction Plan

## Goal

Reduce entropy in `canon-mini-agent` by treating the codebase as a reducible
graph system rather than a collection of unrelated detectors.

The objective is not "find more smells". The objective is:

- centralize control
- centralize mutation
- centralize persistent writes
- centralize error shaping
- reduce wrapper indirection
- reduce duplicate control regions
- reduce implicit state machines
- verify that graph entropy decreases after each accepted refactor

This document defines the implementation plan, data model, scoring model,
report taxonomy, and acceptance criteria.

## North Star

Model the codebase as a labeled transition graph.

Each analysis/refactor cycle should:

1. capture structural facts from the compiler and analyzer
2. annotate nodes/edges with effects and state deltas
3. detect reducible regions under explicit proof tiers
4. emit actionable reports
5. apply refactors
6. verify that the reduced graph is strictly better on the chosen metrics

## Non-Goals

This plan does not attempt to:

- replace all repo-health evaluation with graph analysis
- infer human intent from compiler structure alone
- prove semantic equivalence for arbitrary business logic
- centralize policy decisions into the rustc wrapper

The wrapper should remain a fact producer. Architectural judgment stays in the
agent/analyzer.

## Current State

Completed:

- `graph.json` capture for semantic nodes, semantic edges, CFG nodes/edges,
  bridge edges, redundant CFG paths, and alpha-equivalent wrapper pathways
- compiler-side alpha pathway proof tightening
- graph-only complexity scoring from `graph.json`
- targeted pathway issue generation from fresh artifacts
- artifact/state read-write annotations
- error-shaping annotations
- transition annotations
- proof-grade reports:
  - `artifact_writer_dispersion`
  - `error_shaping_dispersion`
- hypothesis-grade reports:
  - `state_transition_dispersion`
  - `planner_loop_fragmentation`
  - `implicit_state_machine`
  - `effect_boundary_leak`
  - `representation_fanout`
- graph verification snapshot + delta reporting

Missing:

- explicit workflow-domain edges from compiler facts rather than symbol classification
- SCC / dominator region reduction candidates
- richer effect classes:
  - process spawn
  - network
  - logging split from generic artifact/state IO
- stronger transition semantics than “branches + writes state”
- tighter proof boundary between canonical boundary modules and leak candidates

## Canonical Object

The canonical analysis object is `GraphVNext`, a labeled transition multigraph.

It has these node classes:

- `symbol`
  - fn / method / const / static / type / module / trait / impl
- `cfg_region`
  - basic block
  - dominator region
  - SCC region
- `state_domain`
  - status enum family
  - workflow stage family
  - state file family
- `artifact_domain`
  - issues
  - objectives
  - plan
  - tlog
  - diagnostics
  - reports/*
- `effect_domain`
  - persistent_write
  - filesystem_read
  - logging
  - process_spawn
  - network
  - error_shaping
  - report_rendering
- `workflow_domain`
  - planner
  - apply
  - verify
  - supervisor

It has these edge classes:

- `Calls`
- `BelongsTo`
- `Entry`
- `Call`
- `ReadsArtifact`
- `WritesArtifact`
- `ReadsState`
- `WritesState`
- `ShapesError`
- `RendersReport`
- `TransitionsState`
- `PlannerStep`
- `ApplyStep`
- `VerifyStep`
- `DelegatesTo`
- `Duplicates`
- `EquivalentPathway`

## Fact Ownership Boundary

### Wrapper-owned facts

These must be emitted by `canon-rustc-v2` or derived mechanically from wrapper
artifacts:

- CFG structure
- call structure
- MIR fingerprints
- duplicate path pairs
- alpha-equivalent pathways
- recursion / SCC raw structure
- exact artifact write/read sites when identifiable
- exact error-shaping callsites when identifiable
- exact transition-like branching over enums/discriminants when identifiable

### Agent-owned judgments

These stay in `canon-mini-agent`:

- scoring
- prioritization
- proof-tier labeling
- architecture recommendations
- canonicality recommendations
- report generation
- rewrite planning
- graph-delta verification policy

Rule:

- wrapper answers "what is structurally true?"
- agent answers "what should we do about it?"

## Layers

### Layer 1: Observation

Produce raw structural facts.

Required outputs:

- semantic graph
- CFG graph
- bridge graph
- call graph
- redundant CFG path pairs
- alpha pathways
- artifact read/write events
- error-shaping sites
- transition-like branch sites
- planner/apply/verify workflow edges

Deliverables:

- extend `graph.json`
- version schema
- preserve backward compatibility where possible

Acceptance:

- all structural facts are derivable without source-text heuristics
- emitted artifacts are deterministic for unchanged code

### Layer 2: Semantic Annotation

Annotate raw facts into domains the reduction system can reason about.

Required annotations:

- effect class per symbol
- artifact family per read/write
- error-shaping family per symbol
- state-domain identity
- transition delta hints
- representation domain crossings
- ownership domain tags

Examples:

- a function writing `agent_state/ISSUES.json` is attached to `artifact_domain:issues`
- a function calling `map_err/context/with_context/format!` around an error path is
  attached to `effect_domain:error_shaping`
- a function branching on the same enum/status family contributes to one
  `state_domain`

Acceptance:

- annotations are explicit in report input data
- no report needs to infer core domains from prose

### Layer 3: Reduction Operators

These are the allowed simplification moves.

Proof-grade operators:

- wrapper elimination
- duplicate MIR body collapse candidates
- redundant CFG path folding candidates
- exact writer dispersion collapse candidates
- exact error-shaping dispersion collapse candidates

Hypothesis-grade operators:

- state transition centralization
- implicit state machine extraction
- dominator region collapse
- bisimilar transition fragment collapse
- planner/apply/verify loop centralization
- representation fanout collapse

Acceptance:

- every operator declares input facts, proof level, and expected graph delta

### Layer 4: Scoring

We maintain a graph-only entropy score independent of repo-health evaluation.

Current graph-only function:

- local branch score
- statement density
- transitive branch score
- heat
- duplicate-body pressure
- redundant-path pressure
- pathway pressure
- SCC pressure

This is useful but incomplete.

We still need dispersion scores for:

- artifact writer dispersion
- state transition dispersion
- error shaping dispersion
- planner/apply/verify fragmentation
- effect boundary leaks

Target aggregate:

`entropy_score = control + mutation + effect + representation + indirection`

More concretely:

- `control_entropy`
  - branch score
  - transitive branching
  - SCC pressure
  - implicit state machine density
- `mutation_entropy`
  - number of state/artifact mutation sites per domain
- `effect_entropy`
  - side-effect dispersion
  - boundary leakage
  - error-shaping dispersion
- `representation_entropy`
  - number of translation sites per domain pair
- `indirection_entropy`
  - wrapper chains
  - duplicate pathways

Acceptance:

- scores are computed from graph/annotation data only
- lower score after refactor must correspond to a measurable structural simplification

### Layer 5: Reporting

Reports are split by proof tier.

Proof-grade report kinds:

- `pathway_elimination`
- `redundant_path`
- `duplicate_body`
- `artifact_writer_dispersion`
- `error_shaping_dispersion`

Hypothesis-grade report kinds:

- `state_transition_dispersion`
- `implicit_state_machine`
- `planner_loop_fragmentation`
- `effect_boundary_leak`
- `representation_fanout`

Every report must include:

- observed dispersion/redundancy
- canonical target recommendation
- evidence lines
- required refactor steps
- acceptance criteria
- expected graph delta
- proof tier

Acceptance:

- report wording is actionable, not descriptive only
- proof-grade reports can be executed mechanically with verification

### Layer 6: Verification

Every accepted refactor should be evaluated by graph deltas.

Required before/after metrics:

- `overall_graph_entropy_score`
- branch score of touched symbols
- SCC size of touched workflow regions
- pathway count
- redundant path count
- artifact writer dispersion count
- error-shaping dispersion count
- state transition dispersion count

Verification rule:

- a refactor is accepted only if build/tests pass and graph metrics improve or are
  explicitly justified

## Report Taxonomy

### 1. `artifact_writer_dispersion`

Problem:

- one artifact family written by multiple non-canonical sites

Evidence:

- `WritesArtifact` edges grouped by artifact domain

Action:

- redirect writes to one canonical writer
- delete or downgrade wrapper writers

Proof tier:

- proof-grade if writes are exact and artifact domain is exact

### 2. `error_shaping_dispersion`

Problem:

- error text/context/report shaping spread across many symbols

Evidence:

- `ShapesError` edges
- repeated context chains

Action:

- route through one error classification / shaping layer

Proof tier:

- proof-grade when shape sites are exact
- otherwise hypothesis-grade

### 3. `state_transition_dispersion`

Problem:

- same state domain mutated in multiple unrelated locations

Evidence:

- `TransitionsState` edges grouped by state domain

Action:

- extract canonical transition engine

Proof tier:

- hypothesis-grade until transition labeling is trustworthy

### 4. `planner_loop_fragmentation`

Problem:

- planner/apply/verify spread across multiple non-canonical orchestrators

Evidence:

- workflow-domain edges

Action:

- collapse to one canonical loop

Proof tier:

- hypothesis-grade

### 5. `implicit_state_machine`

Problem:

- one function encodes a state machine through branching/loops without explicit state type

Evidence:

- repeated discriminant branching
- back-edges
- repeated transition-like calls

Action:

- extract enum + transition table

Proof tier:

- hypothesis-grade

## Milestones

### Milestone 1: Graph VNext Schema

Implement:

- schema extension for artifact IO edges
- schema extension for error-shaping edges
- schema extension for transition edges
- schema versioning

Acceptance:

- `graph.json` contains raw facts for writes, error shaping, and transition candidates

### Milestone 2: Dispersion Annotations

Implement:

- artifact domain mapping
- error-shaping family grouping
- state-domain grouping

Acceptance:

- grouped dispersion metrics can be computed without source-text heuristics

### Milestone 3: First Anti-Entropy Reports

Implement:

- `artifact_writer_dispersion`
- `error_shaping_dispersion`
- `state_transition_dispersion`

Acceptance:

- reports emitted with evidence, canonical target, and proof tier

Status:

- complete

### Milestone 4: Reduction Verification

Implement:

- before/after graph delta snapshot
- entropy delta summary
- per-report reduction delta checks

Acceptance:

- accepted refactors show measurable graph simplification

Status:

- complete

### Milestone 5: Workflow / State-Machine Reports

Implement:

- `planner_loop_fragmentation`
- `implicit_state_machine`
- SCC / dominator reduction candidates

Acceptance:

- workflow/state reports are emitted from graph/annotation data

Status:

- partially complete
- implemented:
  - `planner_loop_fragmentation`
  - `implicit_state_machine`
  - `effect_boundary_leak`
  - `representation_fanout`
- still missing:
  - SCC / dominator reduction candidates

## Near-Term Implementation Order

Do this next, in order:

1. add explicit workflow-domain edges in the compiler facts
2. replace symbol-name workflow classification in `planner_loop_fragmentation`
3. add SCC / dominator region summaries to `graph.json`
4. implement reduction-candidate reports from those summaries
5. refine effect classes beyond generic state/artifact IO
6. tighten `state_transition_dispersion` with richer transition semantics

Do not:

- add more isolated smell detectors before the remaining graph fact layers land
- keep expanding score formulas without new fact layers
- move policy decisions into the wrapper

## Acceptance Criteria For The Plan

This plan is complete enough when:

- every anti-entropy objective maps to a graph fact or annotation
- every report kind has a proof tier
- every refactor class has a measurable graph delta
- the wrapper/agent boundary is stable
- the next three milestones can be implemented without redefining the architecture

## Immediate Next Step

Start with explicit workflow-domain graph facts.

Reason:

- current `planner_loop_fragmentation` still depends on symbol classification
- workflow edges should be facts, not naming conventions
- SCC / dominator reduction work will need the workflow region boundaries anyway

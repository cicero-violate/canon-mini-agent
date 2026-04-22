  What you actually need
  You need 5 layers:

  1. Observation layer

  - full structural graph
  - CFG
  - call graph
  - effect graph
  - state-touch graph
  - artifact/write graph
  - error-shaping graph
  - planner/apply/verify loop graph

  2. Semantic annotation layer

  - state deltas
  - effect classes
  - transition labels
  - representation changes
  - boundary crossings
  - equivalence hints
  - ownership/canonicality domains

  3. Reduction layer

  - SCC collapse
  - wrapper elimination
  - duplicate-body collapse
  - bisimulation / transition equivalence
  - dominator region hashing
  - state machine minimization
  - writer centralization candidates
  - control choke-point extraction

  4. Scoring layer

  - entropy generation score
  - branch dispersion
  - mutation dispersion
  - control dispersion
  - effect dispersion
  - representation fanout
  - equivalence-compression opportunity
  - estimated entropy reduction after rewrite

  5. Refactor/report layer

  - proof-grade reports
  - hypothesis-grade reports
  - rewrite plans
  - acceptance criteria
  - verification against new graph after refactor

  The key question
  Before implementation, you need to decide this:

  What is the canonical mathematical object?

  It should probably be:

  - a labeled transition multigraph
  - with semantic nodes for functions/modules/state domains/artifacts
  - CFG regions as subgraphs
  - edges labeled with:
      - calls
      - writes
      - reads
      - transitions
      - error shaping
      - boundary adaptation
      - delegation
  - node/edge annotations carrying:
      - effects
      - state deltas
      - branch complexity
      - ownership domains

  Without that, the rest will sprawl.

  So yes, make a plan
  The plan should answer:

  1. What exact graph do we build?
  2. What facts come from rustc-wrapper vs agent-side analysis?
  3. What equivalence relations do we support?
  4. What reductions are proof-grade vs heuristic?
  5. What report classes do we emit?
  6. What is the scoring function?
  7. What is the verification loop after rewrite?

  Best sequence

  1. Define graph schema
  2. Define effect/state annotation schema
  3. Define reduction operators
  4. Define scoring/objective function
  5. Define report taxonomy
  6. Implement proof-grade passes first
  7. Add heuristic passes second
  8. Close with rewrite verification

  My answer

  - Yes, I know what is needed at a systems level.
  - No, it should not be improvised incrementally anymore.
  - Yes, you should create a real plan now.

  If you want, I can write the actual implementation plan next:

  1. architecture
  2. schema
  3. passes
  4. scoring
  5. milestones
  6. acceptance tests

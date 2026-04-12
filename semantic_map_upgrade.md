# Intent: DefId Semantic Graph

## Objective

Construct a deterministic semantic graph:

G = (D, R)

Where:
- Nodes (D) are DefIds
- Edges (E) are labeled relations between DefIds

All meaning is derived via:
- DefId → DefKind (tcx.def_kind)
- HIR (structure)
- MIR (behavior)
- TY (types)

---

## Node Model

Each node is:

- DefId (identity)
- DefKind (metadata)

No string-based identity
No duplication
Canonical compiler-backed graph

---

## Edge Model

Edges are:

(D_i) --R--> (D_j)

Where R ∈:

- Calls        (fn → fn)
- Uses         (fn → type/struct/trait)
- Owns         (struct/enum → field types)
- Returns      (fn → return type)
- Implements   (impl → trait)
- Declares     (trait → fn)
- Defines      (impl → fn)

---

## Edge Derivation

Edges are derived, not stored.

### MIR-derived
- Calls: from terminator::Call
- Uses: operand types
- Returns: destination type

### HIR-derived
- Owns: struct/enum fields
- Declares: trait items
- Defines: impl items

### TY-derived
- Implements: impl → trait mapping
- Uses: generic bounds

---

## Combinatorics

Total possible edges:

|E_max| = |D|^2 * |R|

This is the full search space.

---

## Constraints (Validity Filter)

Not all edges are valid.

Define:

valid(D_i, R, D_j) based on DefKind:

Examples:

- Calls:
  D_i ∈ Fn AND D_j ∈ Fn

- Owns:
  D_i ∈ Struct|Enum AND D_j ∈ Type

- Implements:
  D_i ∈ Impl AND D_j ∈ Trait

- Declares:
  D_i ∈ Trait AND D_j ∈ Fn

Thus:

E_valid ⊂ E_max

---

## Graph Properties

- Directed
- Typed edges
- Heterogeneous nodes
- Deterministic (compiler-derived)

---

## Purpose

- Collapse syntax into semantics
- Unify structure + behavior
- Enable:
  - traversal
  - optimization
  - learning
  - invariant detection

---

## Key Principle

DefId is the only identity.

All semantics are projections:

DefId → (HIR, MIR, TY) → Relations

No external naming system required.

---

## Target Outcome

A minimal, loss-controlled semantic graph:

- Fully compiler-aligned
- No ambiguity
- Extensible relation system
- Suitable for:
  - agent reasoning
  - code transformation
  - invariant learning

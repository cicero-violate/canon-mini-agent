# graph.json Intelligence Tips

The file at `state/rustc/<crate_name>/graph.json` is a 4-layer semantic graph of the
compiled crate. These tips describe how to exploit it effectively during diagnosis and
code review tasks.

---

## What the file contains

| Layer          | Key            | What it holds                                                 |
|----------------+----------------+---------------------------------------------------------------|
| Semantic nodes | `nodes`        | Every fn/struct/enum/trait — path, kind, MIR stats, signature |
| Semantic edges | `edges`        | `Calls`, `Contains`, `Uses`, `Returns`, `Implements`, etc.    |
| CFG nodes      | `cfg_nodes`    | One entry per basic block: owner fn, terminator, statements   |
| CFG edges      | `cfg_edges`    | Intra-function control flow with back-edge markers            |
| Bridge edges   | `bridge_edges` | `Entry` (fn→bb0), `BelongsTo` (bb→fn), `Call` (bb→callee fn)  |

---

## Tip 1 — Use bridge edges, not semantic edges, for call graphs

Semantic `Calls` edges are HIR-derived and lose resolution for async fns and trait
methods. Bridge `Call` edges come from MIR Call/TailCall terminators and are more
precise. To find what a function actually calls at the machine level:

```python
for e in bridge_edges:
    if e["relation"] == "Call":
        bb = cfg_nodes.get(e["from"], {})
        owner_id = bb.get("owner", "")
        owner_path = id_to_path.get(owner_id, "")
        callee = id_to_path.get(e["to"], e["to"])
```

Note: `e["to"]` is a numeric node ID, not a path. Always resolve via `id_to_path`.

---

## Tip 2 — MIR block/switchint counts reveal structural complexity instantly

```python
for nid, node in nodes.items():
    mir = node.get("mir") or {}
    if mir.get("switchint_count", 0) > 5:
        print(node["path"], "sw=", mir["switchint_count"])
```

High `switchint_count` = complex branching (large match arms).
High `blocks` with low `switchint_count` = deep linear code (long pipelines).
This lets you zero-in on complexity hotspots without reading any source.

---

## Tip 3 — Trace ownership of a lane/state bug via the CFG

When diagnosing a control-flow bug (e.g. "why is this code path never reached"):

1. Find the function in `nodes` by path substring.
2. Collect all its `cfg_nodes` entries (`cfg::<path>::bbN`).
3. Walk `cfg_edges` from bb0 to reconstruct the basic-block graph.
4. Find all `bridge_edges` with `relation=Call` originating from those blocks.

This let us discover that `in_progress && !has_active_tab` lane reset was
**inside** the `if let Some(checkpoint)` block by observing that the block containing
the `LaneInProgressSet` call had no CFG path reachable from the "checkpoint discarded"
branch — confirming the dead-path hypothesis without running the code.

---

## Tip 4 — Cross-reference `in_loop` flag for loop/poll diagnosis

Every `cfg_node` has `"in_loop": true/false`. When a process appears stuck in a
polling loop, identify the poll-sleep block:

```python
for nid, cn in cfg_nodes.items():
    if cn.get("in_loop") and cn.get("terminator") == "Call":
        # look for sleep/yield calls here
```

Combined with tip 2, this pinpoints whether the stuck code is a back-edge loop
(tight spin) vs a tail-sleep poll (500ms timer) vs a true deadlock (blocked async await).

---

## Tip 5 — Identify which branch owns a function's error paths

`cfg_nodes` statements include `kind=Assign` with `written_local` and `read_locals`.
Blocks ending in `terminator=Return` with no preceding `Assign` are early-return
(guard) blocks. Trace the CFG backward from `Return` blocks to find which conditions
produce early exits vs the main path — useful for diagnosing why a recovery path
is skipped.

---

## Tip 6 — Find functions gated inside a conditional block

To check if a function call is conditionally gated (e.g., inside `if let Some(x)`):

1. Find the `cfg_node` whose bridge `Call` edge points to the target function.
2. Walk backward through `cfg_edges` to find the nearest `SwitchInt` predecessor.
3. If the SwitchInt's two successors have asymmetric call presence, the call is
   inside a branch — and the other branch silently skips it.

This is exactly how we found the `load_checkpoint` → `in_progress` reset gap:
the reset appeared only in one branch of the SwitchInt that guards the `Option` match.

---

## Tip 7 — `redundant_paths` exposes dead/duplicate branches

`graph.json` now has a `redundant_paths` array. Each entry is a pair of paths
through the same function's CFG with identical polynomial path signatures. These
are candidates for dead code, symmetric match arms, or copy-paste divergence.

```python
for pair in g.get("redundant_paths", []):
    if pair["path_a"]["blocks"] != pair["path_b"]["blocks"]:
        print(pair["path_a"]["owner"], pair["shared_signature"])
```

Filter by path length ≥ 3 blocks to skip trivial match-arm noise.

---

## Tip 8 — Use `nodes.mir.fingerprint` for behavioral diff across builds

Each function stores a SHA-256 `fingerprint` of its MIR shape. If you have two
`graph.json` files (before/after a patch), compare fingerprints to identify exactly
which functions changed behavior — faster and more precise than `git diff`.

```python
before = {n["path"]: n["mir"]["fingerprint"] for n in before_nodes.values() if n.get("mir")}
after  = {n["path"]: n["mir"]["fingerprint"] for n in after_nodes.values()  if n.get("mir")}
changed = [p for p in before if before[p] != after.get(p)]
```

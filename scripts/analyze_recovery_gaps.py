#!/usr/bin/env python3
"""
Recovery gap analyzer — reads graph.json, detects structural recovery gaps,
and appends typed ErrorClass records to blockers.json.

These enter the same pipeline as runtime errors:
  blockers.json → compute_blocker_class_coverage → eval → REPAIR_PLAN → task

Three gap classes detected:

  missing_classification_path
    A route_gate or validation_gate function has no forward path to any
    classifier (classify_result, classify_blocker_summary, etc.) in the
    call graph.  Errors it generates are silently lost.

  unreachable_recovery_dispatch
    A function with "recover" in its path (repair_or_initialize intent) has
    no forward path to the canonical recovery dispatch
    (apply_recovery_decision, record_recovery_triggered, etc.).
    Recovery is ad-hoc and not tracked by eval.

  uncanonicalized_state_transition
    A function has TransitionsState edges but is NOT reachable from
    canonical_writer::apply in the forward call graph.
    State mutation is bypassing the canonical writer.

Usage:
  python3 scripts/analyze_recovery_gaps.py [--workspace PATH] [--dry-run]

  --workspace PATH   project workspace root (default: cwd)
  --dry-run          print gaps instead of writing to blockers.json
"""

import argparse
import hashlib
import json
import os
import sys
import time
from collections import deque
from typing import Dict, List, Optional, Set, Tuple

# ── Error class keys (must match ErrorClass::as_key() in error_class.rs) ──────
EC_MISSING_CLASSIFICATION   = "missing_classification_path"
EC_UNREACHABLE_DISPATCH     = "unreachable_recovery_dispatch"
EC_UNCANONICALIZED_TRANS    = "uncanonicalized_state_transition"

MAX_BLOCKER_RECORDS = 500


# ── Graph loading ──────────────────────────────────────────────────────────────

def load_graph(workspace: str) -> dict:
    path = os.path.join(workspace, "state", "rustc", "canon_mini_agent", "graph.json")
    if not os.path.exists(path):
        print(f"[warn] graph.json not found at {path}", file=sys.stderr)
        return {}
    with open(path) as f:
        return json.load(f)


def load_blockers(workspace: str) -> dict:
    path = os.path.join(workspace, "agent_state", "blockers.json")
    if not os.path.exists(path):
        return {"version": 1, "blockers": []}
    try:
        with open(path) as f:
            return json.load(f)
    except Exception:
        return {"version": 1, "blockers": []}


def write_blockers(workspace: str, data: dict) -> None:
    path = os.path.join(workspace, "agent_state", "blockers.json")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        json.dump(data, f, indent=2)


# ── Graph helpers ──────────────────────────────────────────────────────────────

def build_call_graph(edges: list) -> Tuple[Dict[str, Set[str]], Dict[str, Set[str]]]:
    """Return (forward, reverse) adjacency from Calls edges."""
    forward: Dict[str, Set[str]] = {}
    reverse: Dict[str, Set[str]] = {}
    for e in edges:
        if e.get("relation") != "Calls":
            continue
        frm, to = e["from"], e["to"]
        forward.setdefault(frm, set()).add(to)
        reverse.setdefault(to, set()).add(frm)
    return forward, reverse


def reachable_forward(start_id: str, forward: Dict[str, Set[str]]) -> Set[str]:
    """BFS forward from start_id; returns all reachable node ids."""
    visited: Set[str] = set()
    queue = deque([start_id])
    while queue:
        nid = queue.popleft()
        if nid in visited:
            continue
        visited.add(nid)
        for succ in forward.get(nid, ()):
            if succ not in visited:
                queue.append(succ)
    return visited


def is_generated(path: str) -> bool:
    """Skip serde-generated, test, and other noise paths."""
    skip = ("::_::_serde::", "::_serde::", "::tests::", "::test::")
    return any(s in path for s in skip)


def fn_nodes(nodes: dict) -> Dict[str, dict]:
    return {
        nid: n for nid, n in nodes.items()
        if n.get("kind") == "fn" and not is_generated(n.get("path", ""))
    }


# ── Gap 1: MissingClassificationPath ─────────────────────────────────────────

CLASSIFIER_PATHS = (
    # Direct classification
    "::classify_result",
    "::classify_blocker_summary",
    "::classify_blocker_summary_match",
    "::classify_action_kind_failure",
    "::classify_route_gate_reason",
    "::invalid_route_class",
    # Recording classified errors into blockers.json
    "record_action_failure_with_writer",
    "record_blocker_message_with_writer",
    "::append_blocker",
    # Canonical recovery dispatch (route gates feed here after classification)
    "apply_recovery_decision",
    "record_recovery_triggered",
    "record_recovery_suppressed",
)

# Only flag route_gate — validation_gate functions are utility validators
# whose failures are handled by callers; they don't produce runtime blockers.
GATE_INTENTS = {"route_gate"}

# Skip route_gate helper functions that are clearly utility/counting/state,
# not actual dispatch points that need to classify failures.
ROUTE_GATE_SKIP_SUFFIXES = (
    "_count", "_state", "_payload", "_record",
    "annotate_", "recent_", "executor_route_gate_state",
)


def find_missing_classification_gaps(
    nodes: dict,
    edges: list,
    forward: Dict[str, Set[str]],
) -> List[dict]:
    """
    Find route_gate functions (in the orchestrator app:: module) that have no
    forward path to any classifier or the canonical recovery dispatch.
    These are the actual routing decision points; if they can't reach a
    classifier, runtime errors are silently discarded.
    """
    fns = fn_nodes(nodes)

    # Target ids: any node whose path contains a classifier or dispatch suffix
    target_ids: Set[str] = {
        nid for nid, n in fns.items()
        if any(c in n.get("path", "") for c in CLASSIFIER_PATHS)
    }
    if not target_ids:
        return []

    gaps = []
    for nid, n in fns.items():
        intent = n.get("intent_class", "")
        if intent not in GATE_INTENTS:
            continue
        path = n.get("path", "")
        # Only flag orchestrator-level route gates (app:: module)
        if not path.startswith("app::"):
            continue
        # Skip utility helpers that are not dispatch points
        fn_name = path.rsplit("::", 1)[-1]
        if any(fn_name.startswith(s) or fn_name.endswith(s)
               for s in ROUTE_GATE_SKIP_SUFFIXES):
            continue
        # Don't flag functions that are themselves classifiers/dispatchers
        if any(c in path for c in CLASSIFIER_PATHS):
            continue
        reachable = reachable_forward(nid, forward)
        if not reachable.intersection(target_ids):
            gaps.append({
                "fn_path": path,
                "intent": intent,
                "file": n.get("def", {}).get("file", ""),
                "line": n.get("def", {}).get("line", 0),
            })
    return gaps


# ── Gap 2: UnreachableRecoveryDispatch ────────────────────────────────────────

RECOVERY_DISPATCH_PATHS = (
    "apply_recovery_decision",
    "record_recovery_triggered",
    "record_recovery_outcome",
    "record_recovery_suppressed",
)

REPAIR_INTENT = "repair_or_initialize"


def find_unreachable_dispatch_gaps(
    nodes: dict,
    forward: Dict[str, Set[str]],
) -> List[dict]:
    """
    Find repair_or_initialize functions that contain "recover" in their path
    but have no forward path to the canonical recovery dispatch.
    """
    fns = fn_nodes(nodes)

    target_ids: Set[str] = {
        nid for nid, n in fns.items()
        if any(d in n.get("path", "") for d in RECOVERY_DISPATCH_PATHS)
    }
    if not target_ids:
        return []

    gaps = []
    for nid, n in fns.items():
        path = n.get("path", "")
        intent = n.get("intent_class", "")
        # Focus: functions with recover/recovery in their name
        if intent != REPAIR_INTENT:
            continue
        if "recover" not in path.lower():
            continue
        # Skip if it IS a dispatch target
        if any(d in path for d in RECOVERY_DISPATCH_PATHS):
            continue
        reachable = reachable_forward(nid, forward)
        if not reachable.intersection(target_ids):
            gaps.append({
                "fn_path": path,
                "intent": intent,
                "file": n.get("def", {}).get("file", ""),
                "line": n.get("def", {}).get("line", 0),
            })
    return gaps


# ── Gap 3: UncanonicalizedStateTransition ─────────────────────────────────────

CANONICAL_WRITER_PATHS = ("canonical_writer::",)
TRANSITIONS_STATE_RELATION = "TransitionsState"


def find_uncanonicalized_transition_gaps(
    nodes: dict,
    edges: list,
    forward: Dict[str, Set[str]],
) -> List[dict]:
    """
    Find functions that have TransitionsState outgoing edges but are NOT
    reachable from canonical_writer::apply in the call graph.
    A reachable function is OK — it's being called by the canonical path.
    An unreachable function is transitioning state on its own.
    """
    fns = fn_nodes(nodes)

    # canonical_writer source ids
    canonical_ids: Set[str] = {
        nid for nid, n in fns.items()
        if any(c in n.get("path", "") for c in CANONICAL_WRITER_PATHS)
    }
    if not canonical_ids:
        return []

    # All nodes reachable from any canonical_writer function (forward)
    canonical_reachable: Set[str] = set()
    for cid in canonical_ids:
        canonical_reachable |= reachable_forward(cid, forward)

    # Functions that have TransitionsState outgoing edges
    transitions_state_fns: Set[str] = {
        e["from"] for e in edges
        if e.get("relation") == TRANSITIONS_STATE_RELATION
        and e["from"] in fns
    }

    gaps = []
    for nid in transitions_state_fns:
        n = nodes.get(nid, {})
        path = n.get("path", "")
        if is_generated(path):
            continue
        # It's OK if canonical_writer can reach this function
        if nid in canonical_reachable:
            continue
        # Also OK if this function IS a canonical_writer function
        if any(c in path for c in CANONICAL_WRITER_PATHS):
            continue
        gaps.append({
            "fn_path": path,
            "intent": n.get("intent_class", "unknown"),
            "file": n.get("def", {}).get("file", ""),
            "line": n.get("def", {}).get("line", 0),
        })
    return gaps


# ── Blocker record construction ────────────────────────────────────────────────

def stable_id(error_class: str, fn_path: str) -> str:
    h = hashlib.sha256(fn_path.encode()).hexdigest()[:12]
    return f"graph-{error_class.replace('_', '-')}-{h}"


def make_blocker(error_class: str, fn_path: str, summary: str) -> dict:
    return {
        "id": stable_id(error_class, fn_path),
        "error_class": error_class,
        "actor": "graph_analyzer",
        "summary": summary,
        "action_kind": "graph_analysis",
        "source": "graph_analyzer",
        "ts_ms": int(time.time() * 1000),
    }


def build_blockers(
    gap1: List[dict],
    gap2: List[dict],
    gap3: List[dict],
) -> List[dict]:
    blockers = []

    for g in gap1:
        blockers.append(make_blocker(
            EC_MISSING_CLASSIFICATION,
            g["fn_path"],
            f"{g['intent']} function '{g['fn_path']}' has no reachable classifier "
            f"in call graph — errors silently lost (file={g['file']}:{g['line']})",
        ))

    for g in gap2:
        blockers.append(make_blocker(
            EC_UNREACHABLE_DISPATCH,
            g["fn_path"],
            f"repair function '{g['fn_path']}' has no path to canonical recovery dispatch "
            f"— recovery is ad-hoc and untracked (file={g['file']}:{g['line']})",
        ))

    for g in gap3:
        blockers.append(make_blocker(
            EC_UNCANONICALIZED_TRANS,
            g["fn_path"],
            f"function '{g['fn_path']}' transitions state without being reachable from "
            f"canonical_writer::apply — structural loophole (file={g['file']}:{g['line']})",
        ))

    return blockers


# ── Merge into blockers.json ───────────────────────────────────────────────────

def merge_blockers(existing: dict, new_blockers: List[dict]) -> Tuple[dict, int]:
    """
    Append new blockers (deduplicating by stable id).
    Respects the MAX_BLOCKER_RECORDS cap by dropping oldest.
    Returns (updated_file, added_count).
    """
    existing_ids: Set[str] = {b["id"] for b in existing.get("blockers", [])}
    added = 0
    for b in new_blockers:
        if b["id"] not in existing_ids:
            existing.setdefault("blockers", []).append(b)
            existing_ids.add(b["id"])
            added += 1

    # Cap at MAX_BLOCKER_RECORDS
    blockers = existing.get("blockers", [])
    if len(blockers) > MAX_BLOCKER_RECORDS:
        excess = len(blockers) - MAX_BLOCKER_RECORDS
        existing["blockers"] = blockers[excess:]

    return existing, added


# ── Main ───────────────────────────────────────────────────────────────────────

def main() -> None:
    parser = argparse.ArgumentParser(description="Analyze graph.json for recovery gaps.")
    parser.add_argument("--workspace", default=os.getcwd(), help="Workspace root path")
    parser.add_argument("--dry-run", action="store_true", help="Print gaps, don't write")
    args = parser.parse_args()

    workspace = os.path.abspath(args.workspace)
    graph = load_graph(workspace)
    if not graph:
        print("[analyze_recovery_gaps] graph.json not found or empty — skipping", file=sys.stderr)
        sys.exit(0)

    nodes = graph.get("nodes", {})
    edges = graph.get("edges", [])

    print(f"[analyze_recovery_gaps] loaded graph: {len(nodes)} nodes, {len(edges)} edges")

    forward, reverse = build_call_graph(edges)

    gap1 = find_missing_classification_gaps(nodes, edges, forward)
    gap2 = find_unreachable_dispatch_gaps(nodes, forward)
    gap3 = find_uncanonicalized_transition_gaps(nodes, edges, forward)

    total = len(gap1) + len(gap2) + len(gap3)
    print(
        f"[analyze_recovery_gaps] gaps found: "
        f"{len(gap1)} missing_classification, "
        f"{len(gap2)} unreachable_dispatch, "
        f"{len(gap3)} uncanonicalized_transition "
        f"(total={total})"
    )

    new_blockers = build_blockers(gap1, gap2, gap3)

    if args.dry_run:
        print("\n=== DRY RUN — would append to blockers.json ===")
        for b in new_blockers:
            print(f"  [{b['error_class']}] {b['summary'][:120]}")
        return

    existing = load_blockers(workspace)
    updated, added = merge_blockers(existing, new_blockers)
    write_blockers(workspace, updated)

    print(f"[analyze_recovery_gaps] added {added} new blocker records to agent_state/blockers.json")

    # Summary by class
    for ec, gaps in [
        (EC_MISSING_CLASSIFICATION, gap1),
        (EC_UNREACHABLE_DISPATCH, gap2),
        (EC_UNCANONICALIZED_TRANS, gap3),
    ]:
        if gaps:
            print(f"  {ec}: {len(gaps)} gaps")
            for g in gaps[:5]:
                print(f"    - {g['fn_path']}")
            if len(gaps) > 5:
                print(f"    ... and {len(gaps) - 5} more")


if __name__ == "__main__":
    main()

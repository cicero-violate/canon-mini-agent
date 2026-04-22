#!/usr/bin/env python3
import argparse
import json
import re
from collections import defaultdict
from pathlib import Path


WORKSPACE_SRC_MARKER = "/src/"


def load_graph(path: Path) -> dict:
    with path.open() as f:
        return json.load(f)


def sanitize_id(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9_]", "_", value)


def sanitize_filename(value: str) -> str:
    value = value.strip().replace("::", "__")
    value = re.sub(r"[^A-Za-z0-9._-]", "_", value)
    value = re.sub(r"_+", "_", value).strip("_")
    return value or "item"


def owner_entry(nodes: dict, owner_id: str) -> dict:
    return nodes.get(owner_id, {})


def owner_label(nodes: dict, owner_id: str) -> str:
    entry = owner_entry(nodes, owner_id)
    return entry.get("path") or entry.get("item") or owner_id


def owner_source_file(nodes: dict, owner_id: str) -> str | None:
    entry = owner_entry(nodes, owner_id)
    return (entry.get("def") or {}).get("file")


def owner_is_in_src(nodes: dict, owner_id: str) -> bool:
    file_path = owner_source_file(nodes, owner_id)
    return bool(file_path and WORKSPACE_SRC_MARKER in file_path)


def owner_src_relpath(nodes: dict, owner_id: str) -> str | None:
    file_path = owner_source_file(nodes, owner_id)
    if not file_path or WORKSPACE_SRC_MARKER not in file_path:
        return None
    return file_path.split(WORKSPACE_SRC_MARKER, 1)[1]


def select_owners(graph: dict, owner_substr: str | None, limit: int, src_only: bool = False) -> list[tuple[str, str]]:
    nodes = graph["nodes"]
    cfg_nodes = graph["cfg_nodes"]
    seen = {}
    for cfg in cfg_nodes.values():
        owner_id = cfg["owner"]
        if src_only and not owner_is_in_src(nodes, owner_id):
            continue
        seen.setdefault(owner_id, owner_label(nodes, owner_id))
    owners = sorted(seen.items(), key=lambda kv: kv[1])
    if owner_substr:
        needle = owner_substr.lower()
        owners = [kv for kv in owners if needle in kv[1].lower()]
    return owners[:limit]


def select_owners_by_src_path(graph: dict, src_path_substr: str, limit: int) -> list[tuple[str, str]]:
    nodes = graph["nodes"]
    cfg_nodes = graph["cfg_nodes"]
    seen = {}
    needle = src_path_substr.lower()
    for cfg in cfg_nodes.values():
        owner_id = cfg["owner"]
        rel = owner_src_relpath(nodes, owner_id)
        if not rel or needle not in rel.lower():
            continue
        seen.setdefault(owner_id, owner_label(nodes, owner_id))
    owners = sorted(seen.items(), key=lambda kv: kv[1])
    return owners[:limit]


def build_dot(graph: dict, owners: list[tuple[str, str]], include_calls: bool = True) -> str:
    cfg_nodes = graph["cfg_nodes"]
    cfg_edges = graph["cfg_edges"]
    owner_ids = {oid for oid, _ in owners}

    selected_cfg = {
        cfg_id: cfg
        for cfg_id, cfg in cfg_nodes.items()
        if cfg["owner"] in owner_ids
    }
    selected_cfg_ids = set(selected_cfg)

    lines = [
        "digraph MIR_CFG {",
        '  rankdir=LR;',
        '  graph [fontname="monospace"];',
        '  node [shape=box, fontname="monospace"];',
        '  edge [fontname="monospace"];',
    ]

    for owner_id, owner_name in owners:
        cluster = sanitize_id(owner_id)
        lines.append(f'  subgraph cluster_{cluster} {{')
        lines.append(f'    label="{owner_name}";')
        owner_cfg = [
            (cfg_id, cfg)
            for cfg_id, cfg in selected_cfg.items()
            if cfg["owner"] == owner_id
        ]
        owner_cfg.sort(key=lambda kv: kv[1].get("block", 0))
        for cfg_id, cfg in owner_cfg:
            term = cfg.get("terminator", "?")
            block = cfg.get("block", "?")
            cleanup = " cleanup" if cfg.get("is_cleanup") else ""
            in_loop = " loop" if cfg.get("in_loop") else ""
            label = f"bb{block}\\n{term}{cleanup}{in_loop}"
            lines.append(f'    "{cfg_id}" [label="{label}"];')
        lines.append("  }")

    for edge in cfg_edges:
        src = edge["from"]
        dst = edge["to"]
        if src not in selected_cfg_ids or dst not in selected_cfg_ids:
            continue
        rel = edge.get("relation", "")
        if rel == "Call" and not include_calls:
            continue
        attrs = []
        label = rel
        if edge.get("is_back_edge"):
            label += " back"
            attrs.append('color="crimson"')
            attrs.append("penwidth=2")
        if label:
            attrs.append(f'label="{label}"')
        attr_text = " [" + ", ".join(attrs) + "]" if attrs else ""
        lines.append(f'  "{src}" -> "{dst}"{attr_text};')

    lines.append("}")
    return "\n".join(lines) + "\n"


def batch_export_src(graph: dict, out_dir: Path, include_calls: bool = True) -> tuple[int, Path]:
    nodes = graph["nodes"]
    cfg_nodes = graph["cfg_nodes"]
    owners_by_file: dict[str, list[tuple[str, str]]] = defaultdict(list)
    seen = set()
    for cfg in cfg_nodes.values():
        owner_id = cfg["owner"]
        if owner_id in seen:
            continue
        seen.add(owner_id)
        rel = owner_src_relpath(nodes, owner_id)
        if not rel:
            continue
        owners_by_file[rel].append((owner_id, owner_label(nodes, owner_id)))

    out_dir.mkdir(parents=True, exist_ok=True)
    index_lines = ["# MIR CFG DOT export", ""]
    total = 0
    for rel in sorted(owners_by_file):
        rel_dir = out_dir / Path(rel).parent
        rel_dir.mkdir(parents=True, exist_ok=True)
        file_stem = Path(rel).stem
        index_lines.append(f"## {rel}")
        for owner in sorted(owners_by_file[rel], key=lambda kv: kv[1]):
            total += 1
            dot = build_dot(graph, [owner], include_calls=include_calls)
            name = sanitize_filename(owner[1])
            out_path = rel_dir / f"{file_stem}__{name}.dot"
            out_path.write_text(dot)
            index_lines.append(f"- `{owner[1]}` -> `{out_path.relative_to(out_dir)}`")
        index_lines.append("")

    index_path = out_dir / "INDEX.md"
    index_path.write_text("\n".join(index_lines) + "\n")
    return total, index_path


def main() -> None:
    parser = argparse.ArgumentParser(description="Emit Graphviz DOT from canon-mini-agent MIR CFG graph.json")
    parser.add_argument("graph", type=Path, help="Path to graph.json")
    parser.add_argument("--owner-substr", default=None, help="Case-insensitive substring match on function path")
    parser.add_argument("--src-path-substr", default=None, help="Case-insensitive substring match on src relative path")
    parser.add_argument("--limit", type=int, default=1, help="Maximum number of matching owners to export")
    parser.add_argument("--src-only", action="store_true", help="Limit owner listing/export to items defined under src/")
    parser.add_argument("--all-src", action="store_true", help="Export one DOT per owner for every item defined under src/")
    parser.add_argument("--out-dir", type=Path, default=None, help="Directory for batch export with --all-src")
    parser.add_argument("--no-calls", action="store_true", help="Hide cfg edges with relation=Call")
    parser.add_argument("--list-owners", action="store_true", help="List matching owners and exit")
    parser.add_argument("--out", type=Path, default=None, help="Write DOT to a file instead of stdout")
    args = parser.parse_args()

    graph = load_graph(args.graph)
    include_calls = not args.no_calls

    if args.all_src:
        if args.out is not None:
            raise SystemExit("--out is not supported with --all-src; use --out-dir")
        out_dir = args.out_dir or Path("mir_cfg_dot")
        total, index_path = batch_export_src(graph, out_dir, include_calls=include_calls)
        print(f"exported {total} DOT files under {out_dir}")
        print(index_path)
        return

    if args.src_path_substr:
        owners = select_owners_by_src_path(graph, args.src_path_substr, args.limit)
    else:
        owners = select_owners(graph, args.owner_substr, args.limit, src_only=args.src_only)
    if args.list_owners:
        for owner_id, label in owners:
            print(f"{owner_id}\t{label}")
        return
    if not owners:
        raise SystemExit("no owners matched")
    dot = build_dot(graph, owners, include_calls=include_calls)
    if args.out:
        args.out.write_text(dot)
    else:
        print(dot, end="")


if __name__ == "__main__":
    main()

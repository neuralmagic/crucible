#!/usr/bin/env python3
"""Render the container build-graph from the repo's Containerfiles.

Parses every tracked Containerfile's `FROM` / `COPY --from=` / `ARG`-default image refs and renders
docs/build-graph.png: each recipe is a node (the image it `# produces:`), edges point at the bases it
depends on; internal edges link recipe-to-recipe so the layering chain shows. Personal-namespace nodes
(quay.io/wseaton/*) render red.

`--lint-personal` is the standing guard (no graphviz needed): it fails on any personal-namespace dep
not on the migration allowlist.

Usage:
  build_graph.py            # render docs/build-graph.png (needs graphviz `dot`)
  build_graph.py --lint-personal [--allow-file F]   # exit 1 on personal-namespace nodes not allowed
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PNG_OUT = REPO / "docs" / "build-graph.png"
ALLOW_DEFAULT = REPO / "tools" / "personal-namespace-allowlist.txt"

# A node's namespace class drives its color and the personal-dep lint.
PERSONAL_PREFIXES = ("quay.io/wseaton/",)
# Our own org registry (the migration target): recipes should `produce` here.
ORG_PREFIXES = ("ghcr.io/neuralmagic/", "quay.io/neuralmagic/")
# Public/upstream bases we depend on but don't own (kept, just must stay pullable). Matched against the
# tag-stripped key, so Docker official images appear as bare names (`golang`, `alpine`).
PUBLIC_HINTS = (
    "golang",
    "alpine",
    "registry.access.redhat.com/",
    "gcr.io/",
    "docker.io/",
    "nvidia/",
    "vllm/",
)


class Cls:
    ORG = "org"  # our recipe, org registry (good)
    PERSONAL = "personal"  # personal namespace (the thing to kill)
    EXTERNAL_ORG = "external_org"  # someone else's org (e.g. quay.io/aipcc/*)
    PUBLIC = "public"  # upstream public base


@dataclass
class Recipe:
    """One Containerfile and what it builds."""

    path: str
    produces: str | None  # image identity from `# produces:`, or None
    deps: list[tuple[str, str]] = field(default_factory=list)  # (full_ref, kind: from|copy)


def tracked_containerfiles() -> list[Path]:
    """Only git-tracked recipes — excludes the gitignored vendored upstream checkouts."""
    out = subprocess.run(
        ["git", "-C", str(REPO), "ls-files"], capture_output=True, text=True, check=True
    ).stdout.splitlines()
    names = re.compile(r"(^|/)(Containerfile|Dockerfile)")
    return sorted(REPO / p for p in out if names.search(p))


def image_key(ref: str) -> str:
    """Identity of an image ref: registry/repo with the tag/digest stripped."""
    # strip @sha256:... then :tag (but not the registry's :port — repos always have a / after host)
    ref = ref.split("@", 1)[0]
    if ":" in ref:
        head, tail = ref.rsplit(":", 1)
        if "/" not in tail:  # it's a tag, not a host:port
            ref = head
    return ref


def classify(key: str) -> str:
    if key.startswith(PERSONAL_PREFIXES):
        return Cls.PERSONAL
    if key.startswith(ORG_PREFIXES):
        return Cls.ORG
    if any(h in key or key.startswith(h) for h in PUBLIC_HINTS):
        return Cls.PUBLIC
    return Cls.EXTERNAL_ORG


def parse(path: Path) -> Recipe:
    produces: str | None = None
    args: dict[str, str] = {}
    stages: set[str] = set()
    deps: list[tuple[str, str]] = []

    def resolve(ref: str) -> str:
        m = re.fullmatch(r"\$\{?(\w+)\}?", ref)
        return args.get(m.group(1), ref) if m else ref

    for raw in path.read_text().splitlines():
        line = raw.strip()
        if line.startswith("#"):
            m = re.match(r"#\s*produces:\s*(\S+)", line)
            if m:
                produces = m.group(1)
            continue
        if m := re.match(r"ARG\s+(\w+)=(\S+)", line):
            args[m.group(1)] = m.group(2)
            continue
        if m := re.match(r"FROM\s+(?:--platform=\S+\s+)?(\S+)(?:\s+AS\s+(\S+))?", line, re.I):
            ref = resolve(m.group(1))
            if m.group(2):
                stages.add(m.group(2))
            if ref not in stages:  # FROM a real image, not an earlier stage
                deps.append((ref, "from"))
            continue
        if m := re.search(r"COPY\s+.*--from=(\S+)", line, re.I):
            ref = resolve(m.group(1))
            if ref not in stages:  # COPY from an image, not an earlier stage
                deps.append((ref, "copy"))

    # de-dup deps by (key, kind) keeping first full ref seen
    seen: set[tuple[str, str]] = set()
    uniq: list[tuple[str, str]] = []
    for ref, kind in deps:
        k = (image_key(ref), kind)
        if k not in seen:
            seen.add(k)
            uniq.append((ref, kind))
    return Recipe(path=str(path.relative_to(REPO)), produces=produces, deps=uniq)


def build_model() -> list[Recipe]:
    return [parse(p) for p in tracked_containerfiles()]


def _node_id(key: str) -> str:
    return "n_" + re.sub(r"\W", "_", key)


def _style(cls: str) -> str:
    return {
        Cls.ORG: 'fillcolor="#cfe8d4",color="#2f7d46"',
        Cls.PERSONAL: 'fillcolor="#f4c7c3",color="#c5221f"',
        Cls.EXTERNAL_ORG: 'fillcolor="#fce8b2",color="#b06000"',
        Cls.PUBLIC: 'fillcolor="#e3e6ea",color="#5f6772"',
    }[cls]


def to_dot(recipes: list[Recipe]) -> str:
    # produced-image key -> the recipe node that builds it (for internal edge linking)
    produced: dict[str, Recipe] = {
        image_key(r.produces): r for r in recipes if r.produces
    }

    # Collect every node (recipe products + external leaves), keyed by image identity.
    nodes: dict[str, tuple[str, str]] = {}  # key -> (label, cls)

    def add_node(key: str, label: str, cls: str) -> None:
        # A recipe product wins over a bare leaf for the same key.
        if key not in nodes or cls in (Cls.ORG, Cls.PERSONAL):
            nodes[key] = (label, cls)

    for r in recipes:
        if r.produces:
            k = image_key(r.produces)
            add_node(k, f"{r.produces}\\n({Path(r.path).name})", classify(k))
        else:
            add_node(r.path, r.path, Cls.ORG)

    edges: list[tuple[str, str, str]] = []  # (src_key, dst_key, kind)
    for r in recipes:
        src = image_key(r.produces) if r.produces else r.path
        for ref, kind in r.deps:
            dk = image_key(ref)
            if dk not in produced:  # external leaf node
                add_node(dk, ref, classify(dk))
            edges.append((src, dk, kind))

    lines = [
        "// GENERATED by tools/build_graph.py — do not edit by hand.",
        "// Regenerate with `python3 tools/build_graph.py`; CI fails if this is stale.",
        "digraph build_graph {",
        "  rankdir=LR;",
        '  node [shape=box, style="rounded,filled", fontname="Helvetica", fontsize=10];',
        '  edge [color="#8a929e", fontname="Helvetica", fontsize=8];',
    ]
    for key in sorted(nodes):
        label, cls = nodes[key]
        lines.append(f'  {_node_id(key)} [label="{label}", {_style(cls)}];')
    for src, dst, kind in sorted(set(edges)):
        style = ' [style=dashed, label="copy"]' if kind == "copy" else ""
        lines.append(f"  {_node_id(src)} -> {_node_id(dst)}{style};")
    lines.append("}")
    lines.append("")
    return "\n".join(lines)


def render_png() -> None:
    """Render the graph straight to PNG via graphviz `dot` (the .dot stays in-memory, untracked)."""
    dot = to_dot(build_model())
    try:
        subprocess.run(
            ["dot", "-Tpng", "-o", str(PNG_OUT)], input=dot, text=True, check=True
        )
    except FileNotFoundError:
        sys.exit("graphviz `dot` not found — install it (brew install graphviz / apt-get install graphviz)")


def lint_personal(allow_file: Path | None) -> int:
    allowed: set[str] = set()
    if allow_file and allow_file.exists():
        for line in allow_file.read_text().splitlines():
            line = line.split("#", 1)[0].strip()
            if line:
                allowed.add(line)

    offenders: set[str] = set()
    for r in build_model():
        candidates = [(r.produces, "produces")] if r.produces else []
        candidates += [(ref, kind) for ref, kind in r.deps]
        for ref, _ in candidates:
            if ref is None:
                continue
            key = image_key(ref)
            if classify(key) == Cls.PERSONAL and key not in allowed:
                offenders.add(key)

    if offenders:
        print("PERSONAL-NAMESPACE deps found (not on allowlist):", file=sys.stderr)
        for o in sorted(offenders):
            print(f"  {o}", file=sys.stderr)
        print(
            "\nMigrate these to ghcr.io/neuralmagic/* or add to the allowlist with a reason: "
            f"{ALLOW_DEFAULT.relative_to(REPO)}",
            file=sys.stderr,
        )
        return 1
    print("build-graph: no un-allowlisted personal-namespace deps")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--lint-personal", action="store_true", help="fail on personal-namespace deps")
    ap.add_argument(
        "--allow-file",
        type=Path,
        default=ALLOW_DEFAULT,
        help="allowlist of personal image keys (one per line, # comments ok)",
    )
    args = ap.parse_args()

    if args.lint_personal:
        return lint_personal(args.allow_file)
    render_png()
    print(f"wrote {PNG_OUT.relative_to(REPO)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

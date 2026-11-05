"""Render the seed's build and every edit instance's builds into traces/index.html: what each build
took, how ccache served its compiles, and each build's trace, opened in place."""

import html
import json
import os
import sys
from dataclasses import dataclass, field
from pathlib import Path

import buildtrace

STYLE = (
    ":root { --flow-bg: #edebe2; --flow-surface: #fff; --flow-ink: #100f0e; --flow-muted: #78756a;"
    " --flow-hairline: #c4c0b0; --flow-accent: #1a4785; }"
    "body { margin: 0; padding: 18px 16px; background: var(--flow-bg); color: var(--flow-ink);"
    " font: 14px/1.5 'Public Sans', system-ui, sans-serif; }"
    "table { border-collapse: collapse; background: var(--flow-surface); margin-bottom: 18px; }"
    "th, td { padding: 6px 10px; border-bottom: 1px solid var(--flow-hairline); text-align: left; }"
    "th { color: var(--flow-muted); font-weight: 500; }"
    "details { margin: 6px 0; } summary { cursor: pointer; color: var(--flow-accent); }"
    "iframe { width: 100%; height: 80vh; border: 1px solid var(--flow-hairline); }"
)

OPEN_IN_PLACE = """
const pages = JSON.parse(document.getElementById("pages").textContent);
const bridge = document.getElementById("flow-theme-bridge");
for (const d of document.querySelectorAll("details[data-page]")) {
  d.addEventListener("toggle", () => {
    if (!d.open || d.querySelector("iframe")) return;
    const page = pages[Number(d.dataset.page)];
    const frame = document.createElement("iframe");
    frame.srcdoc = bridge ? page.replace(/<\\/head>/i, () => bridge.outerHTML + "</head>") : page;
    d.append(frame);
  });
}
"""


@dataclass
class Build:
    lane: str
    name: str
    seconds: int | None
    ninja_s: float | None = None
    jobs: dict[str, int] = field(default_factory=dict)
    page: int | None = None

    def output(self) -> dict[str, object]:
        return {
            "lane": self.lane,
            "build": self.name,
            "seconds": self.seconds,
            "ninja_s": self.ninja_s,
            "jobs": self.jobs,
        }


def instances(inputs: Path) -> list[tuple[str, Path]]:
    return sorted(
        (d.name[len("edit[") : -1], d)
        for d in inputs.iterdir()
        if d.is_dir() and d.name.startswith("edit[") and d.name.endswith("]")
    )


def cell(text: object) -> str:
    return f"<td>{html.escape(str(text))}</td>"


def main() -> None:
    inputs = Path(sys.argv[1])
    deps = json.loads(Path(os.environ["CRUCIBLE_INPUTS_FILE"]).read_text())
    builds: list[Build] = []
    pages: list[str] = []

    def add(build: Build, traces: Path, facts: dict[str, object]) -> None:
        builds.append(build)
        ninja_log = traces / f"{build.name}.ninja_log"
        if not ninja_log.exists():
            return
        steps = traces / f"{build.name}.steps.log"
        trace = buildtrace.logs_trace(
            ninja_log,
            traces / f"{build.name}.ccache-stats.log",
            facts,
            steps if steps.exists() else None,
        )
        meta = trace["otherData"]
        ninja = meta["ninja"] if isinstance(meta, dict) else {}
        build.ninja_s, build.jobs, build.page = ninja["ninja_s"], ninja["jobs"], len(pages)
        pages.append(buildtrace.html(trace, f"vLLM build: {build.lane} {build.name}"))
        print(f"{build.lane} {build.name}: {ninja}", file=sys.stderr)

    seconds = deps.get("seed", {}).get("seconds")
    add(Build("seed", "seed", seconds), inputs / "seed" / "traces", {"seconds": seconds})
    for kernel, instance in instances(inputs):
        report = json.loads((instance / "report.json").read_text())
        for b in report.get("builds", []):
            facts: dict[str, object] = {k: b[k] for k in ("build_id", "status", "seconds") if k in b}
            facts["kernel"] = kernel
            add(Build(kernel, str(b["name"]), b.get("seconds")), instance / "traces", facts)

    body = "".join(
        "<tr>"
        + cell(b.lane)
        + cell(b.name)
        + cell("–" if b.seconds is None else f"{b.seconds} s")
        + cell("–" if b.ninja_s is None else f"{b.ninja_s} s")
        + cell(", ".join(f"{k} {v}" for k, v in sorted(b.jobs.items())) or "–")
        + "</tr>"
        for b in builds
    )
    opened = "".join(
        f"<details data-page='{b.page}'><summary>{html.escape(f'{b.lane} {b.name}')}</summary></details>"
        for b in builds
        if b.page is not None
    )
    data = json.dumps(pages).replace("<", "\\u003c")
    out = Path("traces")
    out.mkdir(exist_ok=True)
    (out / "index.html").write_text(
        "<!doctype html><html><head><meta charset='utf-8'><title>vLLM fan-out builds</title>"
        f"<style>{STYLE}</style></head><body><h1 style='font-size:20px'>vLLM fan-out builds</h1>"
        "<table><tr><th>Lane</th><th>Build</th><th>Build call</th><th>ninja</th><th>Jobs by result</th></tr>"
        f"{body}</table>{opened}"
        f"<script type='application/json' id='pages'>{data}</script><script>{OPEN_IN_PLACE}</script>"
        "</body></html>"
    )
    print(json.dumps({"builds": [b.output() for b in builds]}))


if __name__ == "__main__":
    main()

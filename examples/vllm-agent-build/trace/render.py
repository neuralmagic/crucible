"""Render each build the agent reported as traces/<name>.html, from the ninja log and ccache stats
log it fetched out of the build, and traces/index.html comparing them."""

import html
import json
import sys
from pathlib import Path

import buildtrace

inputs = Path(sys.argv[1])
report = json.loads((inputs / "report.json").read_text())
out = Path("traces")
out.mkdir(exist_ok=True)
rows = []
for b in report["builds"]:
    name = b["name"]
    facts = {k: b[k] for k in ("build_id", "status", "seconds") if k in b}
    trace = buildtrace.logs_trace(
        inputs / "traces" / f"{name}.ninja_log",
        inputs / "traces" / f"{name}.ccache-stats.log",
        facts,
    )
    (out / f"{name}.html").write_text(buildtrace.html(trace, f"vLLM build: {name}"))
    ninja = trace["otherData"]["ninja"]
    rows.append((name, b.get("seconds"), ninja["ninja_s"], ninja["jobs"]))
    print(f"traces/{name}.html: {ninja}", file=sys.stderr)

def cell(text: object) -> str:
    return f"<td>{html.escape(str(text))}</td>"

body = "".join(
    "<tr>"
    + cell(name)
    + cell("–" if seconds is None else f"{seconds} s")
    + cell(f"{ninja_s} s")
    + cell(", ".join(f"{k} {v}" for k, v in sorted(jobs.items())))
    + "</tr>"
    for name, seconds, ninja_s, jobs in rows
)
(out / "index.html").write_text(
    "<!doctype html><html><head><meta charset='utf-8'><title>vLLM builds</title><style>"
    ":root { --flow-bg: #edebe2; --flow-surface: #fff; --flow-ink: #100f0e; --flow-muted: #78756a;"
    " --flow-hairline: #c4c0b0; --flow-accent: #1a4785; }"
    "body { margin: 0; padding: 18px 16px; background: var(--flow-bg); color: var(--flow-ink);"
    " font: 14px/1.5 'Public Sans', system-ui, sans-serif; }"
    "table { border-collapse: collapse; background: var(--flow-surface); }"
    "th, td { padding: 6px 10px; border-bottom: 1px solid var(--flow-hairline); text-align: left; }"
    "th { color: var(--flow-muted); font-weight: 500; }"
    "</style></head><body><h1 style='font-size:20px'>vLLM builds</h1><table>"
    "<tr><th>Build</th><th>Build call</th><th>ninja</th><th>Jobs by result</th></tr>"
    f"{body}</table><p style='color:var(--flow-muted)'>Each build's trace is"
    " traces/&lt;build&gt;.html in the run's captured files.</p></body></html>"
)
print(
    json.dumps(
        {
            "builds": [
                {"name": name, "seconds": seconds, "ninja_s": ninja_s, "jobs": jobs}
                for name, seconds, ninja_s, jobs in rows
            ]
        }
    )
)

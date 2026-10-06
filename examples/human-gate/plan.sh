#!/bin/sh
# Propose a training job: a cost chart, the node list, and a note, then the plan as JSON.
python3 - "${GPUS:-8}" <<'PY'
import json, pathlib, sys

gpus = int(sys.argv[1])
per_node, rate, hours = 4, 3.20, 16.0
nodes = [{"node": f"h100-{i + 1}", "gpus": min(per_node, gpus - i * per_node)}
         for i in range((gpus + per_node - 1) // per_node)]
cost = round(gpus * rate * hours, 2)
out = pathlib.Path("out")
out.mkdir(exist_ok=True)

(out / "nodes.csv").write_text(
    "node,gpus,usd\n" + "".join(f"{n['node']},{n['gpus']},{n['gpus'] * rate * hours:.2f}\n" for n in nodes)
)
(out / "plan.md").write_text(
    f"# Fine-tune llama-8b\n\n- {gpus} × H100 across {len(nodes)} nodes\n- {hours:g} h at ${rate:.2f}/GPU-hour\n"
)
data = json.dumps([{"label": n["node"], "usd": round(n["gpus"] * rate * hours, 2)} for n in nodes])
(out / "cost.html").write_text(f"""<!doctype html>
<meta charset="utf-8">
<style>
  body {{ font: 13px system-ui, sans-serif; margin: 16px; color: #222; }}
  .bar {{ fill: #e8590c; }} .bar:hover {{ fill: #ffb020; }}
  text {{ font-size: 11px; }}
</style>
<h3>Cost by node</h3>
<p id="hover">Hover a bar.</p>
<svg id="chart" width="480" height="220"></svg>
<script>
  const data = {data};
  const svg = document.getElementById("chart");
  const max = Math.max(...data.map((d) => d.usd));
  data.forEach((d, i) => {{
    const h = (d.usd / max) * 160, x = 40 + i * 90;
    const bar = document.createElementNS("http://www.w3.org/2000/svg", "rect");
    Object.entries({{ x, y: 190 - h, width: 60, height: h, class: "bar" }}).forEach(([k, v]) => bar.setAttribute(k, v));
    bar.addEventListener("mouseenter", () => {{ document.getElementById("hover").textContent = `${{d.label}}: $${{d.usd}}`; }});
    svg.appendChild(bar);
    const label = document.createElementNS("http://www.w3.org/2000/svg", "text");
    Object.entries({{ x: x + 4, y: 206 }}).forEach(([k, v]) => label.setAttribute(k, v));
    label.textContent = d.label;
    svg.appendChild(label);
  }});
</script>
""")
print(json.dumps({"model": "llama-8b", "gpus": gpus, "hours": hours, "est_usd": cost}))
PY

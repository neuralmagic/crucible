"""Emit the variants that came back dirty."""

import json
import os
from pathlib import Path


def outputs_of(entry: object) -> dict[str, dict[str, str]]:
    if isinstance(entry, dict):
        inner = entry.get("result", entry)
        if isinstance(inner, dict):
            outputs = inner.get("outputs", {})
            if isinstance(outputs, dict):
                return {
                    key: {k: v for k, v in res.items() if isinstance(v, str)}
                    for key, res in outputs.items()
                    if isinstance(res, dict)
                }
    return {}


def main() -> None:
    inputs = json.loads(os.environ.get("CRUCIBLE_INPUTS", "{}"))
    probes = outputs_of(inputs.get("probe", {}))
    dirty = sorted(k for k, v in probes.items() if v.get("status") == "dirty")

    lines = ["# Dirty variants", ""]
    if dirty:
        for key in dirty:
            lines.append(f"- `{key}` — blockers: {probes[key].get('blockers') or 'vendored/missing system OpenSSL'}")
    else:
        lines.append("None. Every probed variant resolves crypto to the system OpenSSL.")
    Path("DIRTY.md").write_text("\n".join(lines) + "\n")

    print(json.dumps({"dirty": dirty}))


if __name__ == "__main__":
    main()

"""Emit one fan-out key per declared build variant."""

import json
from pathlib import Path


def main() -> None:
    variants = json.loads(Path("variants.json").read_text())
    keys = sorted(variants)

    lines = ["# Variants under watch", ""]
    lines += ["| variant | package | target | features |", "| --- | --- | --- | --- |"]
    for key in keys:
        v = variants[key]
        feats = ", ".join(v["features"]) or "-"
        if v.get("default_features", True):
            feats = f"default + {feats}" if v["features"] else "default"
        lines.append(f"| {key} | {v['package']} | {v['target']} | {feats} |")
    Path("VARIANTS.md").write_text("\n".join(lines) + "\n")

    print(json.dumps({"variants": keys}))


if __name__ == "__main__":
    main()

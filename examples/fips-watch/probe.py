"""Resolve one variant's dependency graph and decide whether it is FIPS-clean."""

import json
import os
import subprocess
import sys
from pathlib import Path


def tree(args: list[str]) -> tuple[int, str]:
    proc = subprocess.run(
        ["cargo", "tree", "-e", "normal", *args],
        capture_output=True,
        text=True,
        cwd="checkout",
    )
    return proc.returncode, (proc.stdout or "") + (proc.stderr or "")


NOTHING = "nothing to print"
ABSENT = "did not match any packages"


def resolved(code: int, out: str) -> bool:
    """Whether cargo answered, as opposed to failing to build the graph at all."""
    return code == 0 or NOTHING in out or ABSENT in out


def main() -> None:
    inputs = json.loads(os.environ.get("CRUCIBLE_INPUTS", "{}"))
    key = inputs.get("item")
    if not isinstance(key, str) or not key:
        print("no variant item in CRUCIBLE_INPUTS", file=sys.stderr)
        sys.exit(1)

    variants = json.loads(Path("variants.json").read_text())
    blockers = json.loads(Path("blockers.json").read_text())
    required = json.loads(Path("required.json").read_text())
    v = variants[key]

    base = ["-p", v["package"], "--target", v["target"]]
    if not v.get("default_features", True):
        base.append("--no-default-features")
    if v["features"]:
        base += ["--features", ",".join(v["features"])]

    errors: list[str] = []
    found: dict[str, list[str]] = {}
    for crate in blockers:
        if crate == "rustls":
            continue
        code, out = tree([*base, "-i", crate])
        if not resolved(code, out):
            errors.append(f"{crate}: cargo could not resolve the graph: {out.strip()[:300]}")
            continue
        if code == 0 and out.strip() and NOTHING not in out and ABSENT not in out:
            found[crate] = out.strip().splitlines()[0:12]

    present: dict[str, bool] = {}
    for crate in required:
        code, out = tree([*base, "-i", crate])
        if not resolved(code, out):
            errors.append(f"{crate}: cargo could not resolve the graph: {out.strip()[:300]}")
            present[crate] = False
            continue
        present[crate] = (
            code == 0 and bool(out.strip()) and NOTHING not in out and ABSENT not in out
        )

    code, feat = tree([*base, "-i", "openssl-sys", "-f", "{p} {f}"])
    vendored = "vendored" in feat

    if errors:
        Path("PROBE.md").write_text(
            f"# Probe: {key}\n\nStatus: **error**\n\n" + "\n".join(f"- {e}" for e in errors) + "\n"
        )
        print("; ".join(errors), file=sys.stderr)
        sys.exit(1)

    status = "dirty" if (found or vendored or not any(present.values())) else "clean"

    lines = [f"# Probe: {key}", "", f"Status: **{status}**", ""]
    lines += [f"- package: `{v['package']}`", f"- target: `{v['target']}`"]
    lines += [f"- features: `{','.join(v['features']) or 'default'}`", ""]
    if found:
        lines += ["## Blockers in the compiled graph", ""]
        for crate, path in found.items():
            lines += [f"### `{crate}`", f"{blockers[crate]}", "", "```", *path, "```", ""]
    if vendored:
        lines += ["## Vendored OpenSSL", "", "`openssl-sys` carries `vendored`: statically built, not the system module.", ""]
    for crate, ok in present.items():
        lines.append(f"- required `{crate}`: {'present' if ok else 'ABSENT'} — {required[crate]}")
    Path("PROBE.md").write_text("\n".join(lines) + "\n")

    print(json.dumps({"status": status, "blockers": ", ".join(sorted(found)) or ""}))


if __name__ == "__main__":
    main()

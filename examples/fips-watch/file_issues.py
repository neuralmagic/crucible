"""File the tracking issues the triage instances drafted, idempotently."""

import json
import os
import subprocess
import sys
from pathlib import Path

MARKER = "fips-watch-key:"


def gh(args: list[str]) -> tuple[int, str]:
    proc = subprocess.run(["gh", *args], capture_output=True, text=True)
    return proc.returncode, (proc.stdout or "").strip() + (proc.stderr or "").strip()


def main() -> None:
    payloads = json.loads(Path("inputs/roundup/ISSUES.json").read_text() or "[]")
    if not payloads:
        Path("FILED.md").write_text("# Filed tracking issues\n\nNothing to file: every variant is clean.\n")
        print(json.dumps({"filed": 0, "skipped": 0, "reason": "nothing to file"}))
        return

    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if not token:
        Path("FILED.md").write_text(
            "# Filed tracking issues\n\nNothing filed: no GH_TOKEN in the run environment. "
            f"{len(payloads)} payload(s) are in `inputs/roundup/ISSUES.json` to file by hand.\n"
        )
        print(json.dumps({"filed": 0, "skipped": len(payloads), "reason": "no token"}))
        return

    filed, skipped, lines = 0, 0, ["# Filed tracking issues", ""]
    for p in payloads:
        repo, key = p.get("repo"), p.get("dedupe_key")
        title, body = p.get("title"), p.get("body", "")
        if not (repo and key and title):
            skipped += 1
            continue
        body = f"{body}\n\n<!-- {MARKER} {key} -->\n"

        code, out = gh(["issue", "list", "-R", repo, "--state", "all", "--search",
                        f'"{MARKER} {key}" in:body', "--json", "number", "--limit", "1"])
        existing = []
        if code == 0 and out:
            try:
                existing = json.loads(out)
            except json.JSONDecodeError:
                existing = []
        if existing:
            num = existing[0]["number"]
            gh(["issue", "comment", "-R", repo, str(num), "--body",
                "The FIPS watch still reports this on the current tip."])
            lines.append(f"- {repo}#{num} — already open, commented")
            skipped += 1
            continue

        code, out = gh(["issue", "create", "-R", repo, "--title", title, "--body", body])
        if code == 0:
            filed += 1
            lines.append(f"- {out.splitlines()[-1] if out else repo} — filed")
        else:
            skipped += 1
            lines.append(f"- {repo} — FAILED: {out[:200]}")
            print(f"filing failed for {repo}: {out[:300]}", file=sys.stderr)

    Path("FILED.md").write_text("\n".join(lines) + "\n")
    print(json.dumps({"filed": filed, "skipped": skipped}))


if __name__ == "__main__":
    main()

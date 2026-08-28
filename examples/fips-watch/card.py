"""Fold the deterministic FIPS verdict and issue outcome into a bounded card result."""

import json
import os


def output(name: str) -> dict:
    value = json.loads(os.environ.get("CRUCIBLE_INPUTS", "{}")).get(name, {})
    return value if isinstance(value, dict) else {}


def main() -> None:
    verdict = output("roundup")
    filing = output("file")
    dirty = int(verdict.get("dirty", 0))
    blockers = verdict.get("blockers", [])
    blocker_lines = (
        "\n".join(f"- `{blocker}`" for blocker in blockers) if blockers else "- None"
    )
    status = "ACTION REQUIRED" if dirty else "FIPS CLEAN"
    markdown = f"""## FIPS dependency watch: {status}

- Watched revision: `{verdict.get('revision', 'unknown')}`
- Clean variants: **{int(verdict.get('clean', 0))}**
- Dirty variants: **{dirty}**
- Issues filed: **{int(filing.get('filed', 0))}**
- Issues skipped: **{int(filing.get('skipped', 0))}**

### Crypto blockers
{blocker_lines}
"""
    print(
        json.dumps(
            {
                "verdict": "ACTION REQUIRED" if dirty else "FIPS CLEAN",
                "revision": verdict.get("revision", "unknown"),
                "clean_variants": int(verdict.get("clean", 0)),
                "dirty_variants": dirty,
                "crypto_blockers": verdict.get("blockers", []),
                "issues_filed": int(filing.get("filed", 0)),
                "issues_skipped": int(filing.get("skipped", 0)),
                "markdown": markdown,
            }
        )
    )


if __name__ == "__main__":
    main()

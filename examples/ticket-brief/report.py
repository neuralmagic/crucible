"""Stamp REPORT.md from the brief the agent turn captured."""

import json
import os
from datetime import datetime, timezone
from pathlib import Path


def main() -> None:
    inputs = json.loads(os.environ.get("CRUCIBLE_INPUTS", "{}"))
    brief = inputs.get("brief", {})
    if isinstance(brief, dict):
        brief = brief.get("result", brief)
    jira_key = brief.get("jira_key", "?") if isinstance(brief, dict) else "?"
    summary = brief.get("summary", "") if isinstance(brief, dict) else ""

    lines = [
        f"# Brief for {jira_key}",
        "",
        f"Stamped {datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')}.",
        "",
    ]
    captured = Path("inputs/brief/BRIEF.md")
    if captured.exists():
        lines += [captured.read_text().strip(), ""]
    Path("REPORT.md").write_text("\n".join(lines) + "\n")
    print(json.dumps({"jira_key": jira_key, "summary": summary}))


if __name__ == "__main__":
    main()

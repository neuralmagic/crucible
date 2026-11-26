"""Push one commit on top of the upstream base to the fork's fixed branch, and reuse the open pull
request from that branch, opening a draft the first time. Prints the commit and pull request."""

import json
import os
import subprocess
import tempfile
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

NOTE = "docs/crucible-konflux-demo.md"


def inputs() -> dict:
    path = os.environ.get("CRUCIBLE_INPUTS_FILE")
    raw = Path(path).read_text() if path else os.environ.get("CRUCIBLE_INPUTS", "{}")
    return json.loads(raw)


def run(*args: str, cwd: Optional[str] = None) -> str:
    done = subprocess.run(args, cwd=cwd, check=True, capture_output=True, text=True)
    return done.stdout.strip()


def git(work: str, *args: str) -> str:
    return run("git", "-c", "credential.helper=", "-c", "credential.helper=!gh auth git-credential", *args, cwd=work)


def main() -> None:
    p = inputs()["params"]
    fork, upstream, base, branch = p["fork"], p["upstream"], p["base"], p["branch"]
    work = tempfile.mkdtemp(prefix="konflux-pr-")
    run("git", "clone", "--quiet", "--filter=blob:none", "--branch", base,
        f"https://github.com/{upstream}.git", work)
    git(work, "checkout", "--quiet", "-B", branch)
    stamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    run_name = os.environ.get("CRUCIBLE_RUN_NAME", "a crucible run")
    note = Path(work, NOTE)
    note.parent.mkdir(parents=True, exist_ok=True)
    note.write_text(
        "# Crucible Konflux watch demo\n\n"
        f"Pushed by {run_name} at {stamp} to start this pull request's Konflux builds.\n"
    )
    git(work, "add", NOTE)
    git(work, "-c", "user.name=crucible-demo", "-c", "user.email=crucible-demo@users.noreply.github.com",
        "commit", "--quiet", "-m", f"Crucible Konflux watch demo ({stamp})")
    commit = git(work, "rev-parse", "HEAD")
    git(work, "push", "--quiet", "--force", f"https://github.com/{fork}.git", f"HEAD:refs/heads/{branch}")

    owner = fork.split("/")[0]
    listed = json.loads(run("gh", "pr", "list", "-R", upstream, "--head", branch, "--state", "open",
                            "--json", "number,url,headRepositoryOwner"))
    mine = [x for x in listed if x["headRepositoryOwner"]["login"].lower() == owner.lower()]
    opened = not mine
    if opened:
        url = run("gh", "pr", "create", "-R", upstream, "--head", f"{owner}:{branch}", "--base", base,
                  "--draft", "--title", "Crucible Konflux watch demo",
                  "--body", "A demo pull request a crucible playbook updates on every run, to watch "
                            "the Konflux builds it starts. Not for merging.")
        number = int(url.rstrip("/").rsplit("/", 1)[1])
    else:
        number, url = mine[0]["number"], mine[0]["url"]
    print(f"{'opened' if opened else 'updated'} {url} at {commit}", file=sys.stderr)
    print(json.dumps({"commit": commit, "number": number, "url": url, "opened": opened}))


if __name__ == "__main__":
    main()

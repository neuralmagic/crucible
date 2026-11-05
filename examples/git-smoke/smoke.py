"""git-smoke's command side, with stock `git` and `gh` against github.com: in the run pod both
reach GitHub through crucible-git, the same commands work anywhere `gh auth login` has run."""

import argparse
import json
import os
import subprocess
import tempfile
import time
from pathlib import Path

LABEL = "crucible-git-smoke"


def params() -> dict:
    with open(os.environ["CRUCIBLE_INPUTS_FILE"]) as f:
        return json.load(f)["params"]


def run(*args: str, cwd: str | None = None) -> str:
    return subprocess.run(list(args), cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


def push(args: argparse.Namespace) -> None:
    p = params()
    repo, base = p["repo"], p["base"]
    name = os.environ.get("CRUCIBLE_RUN_DISPLAY_NAME", "run")
    branch = f"crucible-git-smoke/{args.who}-{time.strftime('%Y%m%d%H%M%S', time.gmtime())}"
    with tempfile.TemporaryDirectory() as tmp:
        run("git", "clone", "-q", "--depth", "1", "--branch", base, f"https://github.com/{repo}.git", tmp)
        run("git", "checkout", "-q", "-b", branch, cwd=tmp)
        run("git", "-c", "user.name=crucible-git smoke", "-c", "user.email=smoke@crucible.invalid",
            "commit", "-q", "--allow-empty", "-m", f"crucible-git smoke ({name}, {args.who})", cwd=tmp)
        sha = run("git", "rev-parse", "HEAD", cwd=tmp)
        run("git", "push", "-q", "origin", branch, cwd=tmp)
    url = run("gh", "pr", "create", "--repo", repo, "--draft", "--base", base, "--head", branch,
              "--title", f"crucible-git smoke: {args.who}",
              "--body", f"Opened by run {name}'s {args.who} with stock gh through crucible-git.",
              "--label", LABEL).splitlines()[-1]
    out = {"branch": branch, "sha": sha, "pr": url}
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.out).write_text(json.dumps(out, indent=2) + "\n")
    print(json.dumps(out))


def check(args: argparse.Namespace) -> None:
    repo = params()["repo"]
    failures, prs = [], []
    for path in args.evidence:
        try:
            ev = json.loads(Path(path).read_text())
        except (OSError, ValueError) as e:
            failures.append(f"{path}: {e}")
            continue
        if "error" in ev:
            failures.append(f"{path}: {ev['error']}")
            continue
        heads = run("git", "ls-remote", f"https://github.com/{repo}.git", f"refs/heads/{ev['branch']}").split()
        if not heads or heads[0] != ev["sha"]:
            failures.append(f"{path}: {ev['branch']} is {heads[:1]} on GitHub, not {ev['sha']}")
        pr = json.loads(run("gh", "pr", "view", ev["pr"], "--json", "isDraft,headRefName,url"))
        if not pr["isDraft"] or pr["headRefName"] != ev["branch"]:
            failures.append(f"{path}: {ev['pr']} is not a draft from {ev['branch']}: {pr}")
        prs.append(ev["pr"])
    print(json.dumps({"pass": not failures, "score": 1.0 if not failures else 0.0, "prs": prs, "failures": failures}))


def main() -> None:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(required=True)
    p = sub.add_parser("push")
    p.add_argument("--who", required=True)
    p.add_argument("--out", required=True)
    p.set_defaults(func=push)
    c = sub.add_parser("check")
    c.add_argument("evidence", nargs="+")
    c.set_defaults(func=check)
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()

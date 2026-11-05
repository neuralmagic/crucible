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
    proc = subprocess.run(list(args), cwd=cwd, capture_output=True, text=True)
    if proc.returncode != 0:
        raise SystemExit(f"{' '.join(args[:3])} failed ({proc.returncode}): {proc.stderr.strip()[-800:]}")
    return proc.stdout.strip()


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


def attempt(*args: str, cwd: str | None = None) -> subprocess.CompletedProcess:
    env = {**os.environ, "GIT_TERMINAL_PROMPT": "0"}
    return subprocess.run(list(args), cwd=cwd, capture_output=True, text=True, env=env, timeout=120)


def deny(args: argparse.Namespace) -> None:
    """Everything crucible-git must refuse, and the token nowhere a task can read it."""
    p = params()
    repo, base = p["repo"], p["base"]
    failures = []

    def expect_refused(what: str, proc: subprocess.CompletedProcess) -> None:
        if proc.returncode == 0:
            failures.append(f"{what} succeeded: {proc.stdout.strip()[:200]}")

    if Path("/var/run/crucible-git-token").exists():
        failures.append("the token directory is mounted in the loop container")
    if os.environ.get("GH_TOKEN") != "crucible-git":
        failures.append("GH_TOKEN is not the placeholder")
    with tempfile.TemporaryDirectory() as tmp:
        run("git", "clone", "-q", "--depth", "1", "--branch", base, f"https://github.com/{repo}.git", tmp)
        run("git", "-c", "user.name=crucible-git smoke", "-c", "user.email=smoke@crucible.invalid",
            "commit", "-q", "--allow-empty", "-m", "crucible-git smoke: must not land", cwd=tmp)
        expect_refused("a push outside the bound", attempt(
            "git", "push", f"https://github.com/{p['outside_public']}.git",
            "HEAD:refs/heads/crucible-git-smoke-denied", cwd=tmp))
        branch = f"crucible-git-smoke/deny-{time.strftime('%Y%m%d%H%M%S', time.gmtime())}"
        run("git", "push", "-q", "origin", f"HEAD:refs/heads/{branch}", cwd=tmp)
    expect_refused("a pull request past the bound's count", attempt(
        "gh", "pr", "create", "--repo", repo, "--draft", "--base", base, "--head", branch,
        "--title", "crucible-git smoke: must be refused", "--body", "count exhausted"))
    if p.get("outside_private"):
        expect_refused("a read of a private repository outside the bound",
                       attempt("gh", "api", f"repos/{p['outside_private']}"))
    expect_refused("a REST issue comment", attempt(
        "gh", "api", "-X", "POST", f"repos/{repo}/issues/1/comments", "-f", "body=must be refused"))
    expect_refused("a GraphQL mutation outside the allowed set", attempt(
        "gh", "api", "graphql", "-f",
        'query=mutation { addStar(input: {starrableId: "x"}) { clientMutationId } }'))
    print(json.dumps({"pass": not failures, "score": 1.0 if not failures else 0.0, "failures": failures}))


def main() -> None:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(required=True)
    p = sub.add_parser("push")
    p.add_argument("--who", required=True)
    p.add_argument("--out", required=True)
    p.set_defaults(func=push)
    d = sub.add_parser("deny")
    d.set_defaults(func=deny)
    c = sub.add_parser("check")
    c.add_argument("evidence", nargs="+")
    c.set_defaults(func=check)
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()

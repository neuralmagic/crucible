"""git-smoke's command side: push and open a pull request through $CRUCIBLE_GIT_URL, then check
both writers' evidence against GitHub as crucible-git sees it."""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path


def params() -> dict:
    with open(os.environ["CRUCIBLE_INPUTS_FILE"]) as f:
        return json.load(f)["params"]


def base_url() -> str:
    url = os.environ.get("CRUCIBLE_GIT_URL")
    if not url:
        sys.exit("CRUCIBLE_GIT_URL is unset: the run has no crucible-git ([git] in the deploy profile)")
    return url


def git(*args: str, cwd: str | None = None) -> str:
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


def mcp(method: str, params: dict, rid: int) -> dict:
    body = json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}).encode()
    req = urllib.request.Request(
        base_url() + "/mcp",
        data=body,
        headers={"Content-Type": "application/json", "Accept": "application/json, text/event-stream"},
    )
    with urllib.request.urlopen(req, timeout=120) as resp:
        reply = json.load(resp)
    if "error" in reply:
        raise RuntimeError(f"{method}: {reply['error']}")
    return reply["result"]


def open_draft_pr(repo: str, base: str, head: str, title: str, body: str) -> dict:
    mcp(
        "initialize",
        {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "git-smoke", "version": "1"}},
        1,
    )
    result = mcp(
        "tools/call",
        {"name": "open_draft_pr", "arguments": {"repo": repo, "base": base, "head": head, "title": title, "body": body}},
        2,
    )
    if result.get("isError"):
        raise RuntimeError(f"open_draft_pr: {result['content']}")
    return json.loads(result["content"][0]["text"])


def remote(repo: str) -> str:
    return f"{base_url()}/git/{repo}.git"


def push(args: argparse.Namespace) -> None:
    p = params()
    repo, base = p["repo"], p["base"]
    run = os.environ.get("CRUCIBLE_RUN_DISPLAY_NAME", "run")
    branch = f"crucible-git-smoke/{args.who}-{time.strftime('%Y%m%d%H%M%S', time.gmtime())}"
    with tempfile.TemporaryDirectory() as tmp:
        git("clone", "-q", "--depth", "1", "--branch", base, remote(repo), tmp)
        git("checkout", "-q", "-b", branch, cwd=tmp)
        git("-c", "user.name=crucible-git smoke", "-c", "user.email=smoke@crucible.invalid",
            "commit", "-q", "--allow-empty", "-m", f"crucible-git smoke ({run}, {args.who})", cwd=tmp)
        sha = git("rev-parse", "HEAD", cwd=tmp)
        git("push", "-q", "origin", branch, cwd=tmp)
    pr = open_draft_pr(repo, base, branch, f"crucible-git smoke: {args.who}", f"Opened by run {run}'s {args.who} through crucible-git.")
    out = {"branch": branch, "sha": sha, "pr": pr["url"]}
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.out).write_text(json.dumps(out, indent=2) + "\n")
    print(json.dumps(out))


def check(args: argparse.Namespace) -> None:
    p = params()
    repo, base = p["repo"], p["base"]
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
        heads = git("ls-remote", remote(repo), f"refs/heads/{ev['branch']}").split()
        if not heads or heads[0] != ev["sha"]:
            failures.append(f"{path}: {ev['branch']} is {heads[:1]} on GitHub, not {ev['sha']}")
        again = open_draft_pr(repo, base, ev["branch"], "unused", "unused")
        if not again["existing"] or again["url"] != ev["pr"]:
            failures.append(f"{path}: open_draft_pr did not find {ev['pr']} again: {again}")
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

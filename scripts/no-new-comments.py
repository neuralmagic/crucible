"""Refuse comments added to Rust files, other than module docs (`//!`) and the rustdoc of a struct,
enum, trait, type alias or module. Comments are found with ast-grep (tree-sitter), so `//` inside
a string is not a comment. Only added comments count: a comment already in the file, moved or not,
passes, and so does a new line in a comment block that was already there.

  no-new-comments.py --staged      the staged Rust files against HEAD (pre-commit)
  no-new-comments.py --base REF    the working tree's Rust files against REF (CI)
  no-new-comments.py --hook        a Claude Code PreToolUse event on stdin; denies the edit
"""

import json
import re
import shutil
import subprocess
import sys
from collections import Counter
from pathlib import Path
from typing import List, Optional, Tuple

RULE = """id: comment
language: rust
rule:
  any:
    - kind: line_comment
    - kind: block_comment
"""
DOCUMENTED = re.compile(r"^\s*(pub(\([^)]*\))?\s+)?(struct|enum|trait|type|union|mod)\b")
ATTRIBUTE = re.compile(r"^\s*#!?\[")
ALLOWED = (
    "Allowed are `//!` module docs and `///` on a struct, enum, trait, type alias or module; "
    "comments already in the file, and lines added to them, are left alone. Put the why in the "
    "commit message instead."
)


def comments(text: str) -> List[Tuple[int, str]]:
    if not text.strip():
        return []
    done = subprocess.run(
        ["ast-grep", "scan", "--inline-rules", RULE, "--stdin", "--json=compact"],
        input=text,
        capture_output=True,
        text=True,
        timeout=60,
    )
    if done.returncode not in (0, 1):
        raise RuntimeError(done.stderr.strip())
    return [
        (m["range"]["start"]["line"], m["text"].rstrip("\n"))
        for m in json.loads(done.stdout or "[]")
    ]


def allowed(text: str, lines: List[str], line: int) -> bool:
    if text.startswith("//!") or text.startswith("/*!"):
        return True
    if not (text.startswith("///") or text.startswith("/**")):
        return False
    for following in lines[line + 1 :]:
        stripped = following.strip()
        if (
            not stripped
            or stripped.startswith("///")
            or stripped.startswith("*")
            or ATTRIBUTE.match(following)
        ):
            continue
        return bool(DOCUMENTED.match(following))
    return False


def kind(text: str) -> str:
    for prefix in ("///", "//!", "/**", "/*!", "//", "/*"):
        if text.startswith(prefix):
            return prefix
    return ""


def blocks(found: List[Tuple[int, str]]) -> List[List[Tuple[int, str]]]:
    out: List[List[Tuple[int, str]]] = []
    for line, text in sorted(found):
        last = out[-1][-1] if out else None
        if last and last[0] + 1 == line and kind(last[1]) == kind(text):
            out[-1].append((line, text))
        else:
            out.append([(line, text)])
    return out


def added(before: str, after: str) -> List[Tuple[int, str]]:
    had = Counter(text for _, text in comments(before))
    lines = after.split("\n")
    out = []
    for block in blocks(comments(after)):
        kept = [had[text] > 0 for _, text in block]
        for _, text in block:
            if had[text] > 0:
                had[text] -= 1
        if any(kept):
            continue
        for line, text in block:
            if not allowed(text, lines, line):
                out.append((line + 1, text.splitlines()[0][:120]))
    return out


def git(*args: str) -> str:
    return subprocess.run(["git", *args], capture_output=True, text=True, check=True).stdout


def blob(spec: str) -> str:
    done = subprocess.run(["git", "show", spec], capture_output=True, text=True)
    return done.stdout if done.returncode == 0 else ""


def report(found: List[Tuple[str, int, str]]) -> int:
    if not found:
        return 0
    print("no-new-comments: these comments were added:", file=sys.stderr)
    for path, line, text in found:
        print(f"  {path}:{line}: {text}", file=sys.stderr)
    print(ALLOWED, file=sys.stderr)
    return 1


def staged() -> int:
    paths = git("diff", "--cached", "--name-only", "--diff-filter=ACMR", "--", "*.rs").split()
    found = []
    for path in paths:
        found += [(path, n, t) for n, t in added(blob(f"HEAD:{path}"), blob(f":{path}"))]
    return report(found)


def against(base: str) -> int:
    merge_base = git("merge-base", base, "HEAD").strip()
    paths = git("diff", "--name-only", "--diff-filter=ACMR", merge_base, "--", "*.rs").split()
    found = []
    for path in paths:
        after = Path(path).read_text()
        found += [(path, n, t) for n, t in added(blob(f"{merge_base}:{path}"), after)]
    return report(found)


def after_edit(tool: str, given: dict, before: str) -> Optional[str]:
    if tool == "Write":
        return given.get("content", "")
    edits = given.get("edits") if tool == "MultiEdit" else [given]
    after = before
    for edit in edits or []:
        old, new = edit.get("old_string", ""), edit.get("new_string", "")
        if old not in after:
            return None
        after = after.replace(old, new) if edit.get("replace_all") else after.replace(old, new, 1)
    return after


def hook() -> int:
    try:
        event = json.load(sys.stdin)
    except ValueError:
        return 0
    tool = event.get("tool_name", "")
    given = event.get("tool_input", {})
    path = Path(given.get("file_path", ""))
    if path.suffix != ".rs" or tool not in ("Edit", "MultiEdit", "Write"):
        return 0
    before = path.read_text() if path.is_file() else ""
    after = after_edit(tool, given, before)
    if after is None:
        return 0
    try:
        found = added(before, after)
    except (RuntimeError, subprocess.TimeoutExpired, ValueError):
        return 0
    if not found:
        return 0
    reason = "This edit adds comments. " + ALLOWED + " Remove these and retry:\n" + "\n".join(
        f"  {path.name}:{n}: {t}" for n, t in found
    )
    json.dump(
        {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        },
        sys.stdout,
    )
    return 0


def main() -> int:
    if not shutil.which("ast-grep"):
        print("no-new-comments: ast-grep is not installed (brew install ast-grep)", file=sys.stderr)
        return 0 if "--hook" in sys.argv else 1
    if "--hook" in sys.argv:
        return hook()
    if "--staged" in sys.argv:
        return staged()
    if "--base" in sys.argv:
        at = sys.argv.index("--base")
        if at + 1 < len(sys.argv):
            return against(sys.argv[at + 1])
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())

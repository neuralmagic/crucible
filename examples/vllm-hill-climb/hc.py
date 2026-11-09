"""The climb's bookkeeping, so each step is one command.

    python3 hc.py checkout REF         vLLM at REF into build/vllm, tagged `seed`
    python3 hc.py stage LABEL IMAGE    the tree's edits as candidate LABEL on seed IMAGE; prints
                                       the buildit build arguments to use
    python3 hc.py reset                back to the best kept tree
    python3 hc.py keep LABEL           make candidate LABEL the best kept tree
    python3 hc.py mark LABEL EVENT     stamp EVENT for LABEL in timeline.jsonl; `measured` also
                                       records evidence/LABEL.log in results.jsonl

A candidate whose edits are all Python or data under vllm/ builds as one layer over the seed
image: nothing compiles and the seed's layers are neither pulled nor pushed again. Any other edit
(csrc, CMake, setup.py) rebuilds from the Dockerfile through the shared ccache.
"""

import json
import shutil
import subprocess
import sys
import time
from pathlib import Path

SRC = Path("build/vllm")
CAND = Path("cand")
LAYER_SUFFIXES = {".py", ".json", ".txt", ".yaml", ".jinja"}


def git(*args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(SRC), *args], check=True, stdout=subprocess.PIPE, text=True
    ).stdout


def mark(label: str, event: str) -> None:
    if event == "measured":
        log = Path("evidence") / f"{label}.log"
        if not log.is_file():
            sys.exit(f"write the Job's result line to {log} first")
        with open("results.jsonl", "a") as f:
            f.write(json.dumps({"label": label, "log": log.read_text()}) + "\n")
    with open("timeline.jsonl", "a") as f:
        f.write(json.dumps({"label": label, "event": event, "t": round(time.time(), 1)}) + "\n")


def checkout(ref: str) -> None:
    if SRC.exists():
        shutil.rmtree(SRC)
    subprocess.run(["git", "init", "-q", str(SRC)], check=True)
    git("fetch", "-q", "--depth", "1", "https://github.com/vllm-project/vllm.git", ref)
    git("checkout", "-q", "--detach", "FETCH_HEAD")
    git("tag", "seed")
    git("checkout", "-q", "-b", "best")
    for name in ("timeline.jsonl", "results.jsonl", "best.patch"):
        Path(name).touch()


def changed() -> list[tuple[str, str]]:
    git("add", "-A")
    rows = git("diff", "--cached", "--name-status", "--no-renames", "seed").splitlines()
    return [(row.split("\t", 1)[0], row.split("\t", 1)[1]) for row in rows if row.strip()]


def stage(label: str, image: str) -> None:
    if not label.replace("-", "").isalnum() or label != label.lower():
        sys.exit("a label is lowercase letters, digits and -")
    files = changed()
    if not files:
        sys.exit("the tree has no edits over the seed")
    deleted = [path for status, path in files if status == "D"]
    if deleted:
        sys.exit(f"a candidate cannot delete files: {deleted}")
    CAND.mkdir(exist_ok=True)
    (CAND / f"{label}.patch").write_text(git("diff", "--cached", "--binary", "seed"))
    layer = all(
        path.startswith("vllm/") and Path(path).suffix in LAYER_SUFFIXES for _, path in files
    )
    if layer:
        root = CAND / label
        if root.exists():
            shutil.rmtree(root)
        for _, path in files:
            (root / path).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(SRC / path, root / path)
        build = {"context": str(root), "base": image, "script": "true", "path": "/src/vllm"}
    else:
        build = {"context": "build", "dockerfile": "Dockerfile", "ccache": True}
    mark(label, "staged")
    print(
        json.dumps(
            {
                "recipe": "layer" if layer else "dockerfile",
                "files": [path for _, path in files],
                "build": build,
            }
        )
    )


def reset() -> None:
    git("reset", "-q", "--hard", "best")
    git("clean", "-fdq")


def keep(label: str) -> None:
    patch = (CAND / f"{label}.patch").resolve()
    git("reset", "-q", "--hard", "seed")
    git("clean", "-fdq")
    if patch.stat().st_size:
        git("apply", "--binary", str(patch))
    git("add", "-A")
    git("-c", "user.name=hc", "-c", "user.email=hc@localhost", "commit", "-q", "--allow-empty",
        "-m", label)
    shutil.copyfile(patch, "best.patch")
    mark(label, "kept")


def main(argv: list[str]) -> None:
    match argv:
        case ["checkout", ref]:
            checkout(ref)
        case ["stage", label, image]:
            stage(label, image)
        case ["reset"]:
            reset()
        case ["keep", label]:
            keep(label)
        case ["mark", label, event]:
            mark(label, event)
        case _:
            sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])

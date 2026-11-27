"""Print the images the watched Konflux builds pushed, as `url@digest`."""

import json
import os
from pathlib import Path


def inputs() -> dict:
    path = os.environ.get("CRUCIBLE_INPUTS_FILE")
    raw = Path(path).read_text() if path else os.environ.get("CRUCIBLE_INPUTS", "{}")
    return json.loads(raw)


def main() -> None:
    builds = inputs()["builds"]
    images = [
        f"{run['IMAGE_URL']}@{run['IMAGE_DIGEST']}"
        for name, run in sorted(builds.items())
        if name != "runs"
    ]
    print(json.dumps({"images": images}))


if __name__ == "__main__":
    main()

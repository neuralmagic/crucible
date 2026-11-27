"""Emit a fixed config object and label, and write NOTES.md for the child to digest."""

import json
from pathlib import Path

NOTES = """# Release 42

- parser accepts trailing commas
- cache keys include the target triple
- docs: call tasks
"""

Path("NOTES.md").write_text(NOTES)
print(
    json.dumps(
        {
            "label": "release-42",
            "config": {"tags": ["parser", "cache", "docs"], "threshold": 10},
        }
    )
)

params = {
    "label": {
        "type": "string",
        "required": True,
        "doc": "Name the digest is filed under",
    },
    "config": {
        "type": "json",
        "required": True,
        "doc": "A JSON object: tags (list of strings) and threshold (number of words)",
    },
    "notes": {
        "type": "file",
        "required": True,
        "max_bytes": 65536,
        "doc": "Markdown notes to digest",
    },
}

read = command(
    name = "read",
    run = "python3 read.py",
    emits = {"label": "string", "tags": "list", "threshold": "number", "lines": "integer"},
)

digest = command(
    name = "digest",
    run = "python3 digest.py",
    depends_on = [read],
    emits = {
        "label": "string",
        "words": "integer",
        "bullets": "integer",
        "tags": "list",
        "over_threshold": "boolean",
    },
    emits_files = ["DIGEST.md"],
)

workflow(type = "playbook", tasks = [read, digest], result = digest)

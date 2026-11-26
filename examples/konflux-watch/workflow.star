params = {
    "commit": {
        "type": "string",
        "required": True,
        "pattern": "^[0-9a-f]{40}$",
        "doc": "The ModelExpress commit whose pull request builds to wait for",
    },
}

calls = {
    "konflux": {
        "kind": "tekton",
        "cluster": "konflux",
        "namespace": "open-data-hub-tenant",
        "repository": "github-com-opendatahub-io-modelexpress",
        "event": "pull_request",
        "expected": [
            {"name": "odh-modelexpress-on-pull-request", "results": ["IMAGE_URL", "IMAGE_DIGEST"]},
            {"name": "odh-modelexpress-operator-on-pull-request", "results": ["IMAGE_URL", "IMAGE_DIGEST"]},
        ],
    },
}

builds = watch(
    name = "builds",
    target = "konflux",
    commit = param("commit"),
    timeout = "2h",
    appear = "5m",
)

report = command(
    name = "report",
    run = "python3 report.py",
    depends_on = [builds],
    emits = {"images": "list"},
)

workflow(type = "playbook", tasks = [builds, report], result = report)

load("@crucible//calls", "event", "pipeline_run", "tekton")

params = {
    "fork": {"type": "string", "default": "wseaton/modelexpress", "doc": "owner/name the branch is pushed to"},
    "upstream": {"type": "string", "default": "opendatahub-io/modelexpress", "doc": "owner/name the pull request is opened on"},
    "base": {"type": "string", "default": "main", "doc": "the upstream branch the pull request targets"},
    "branch": {"type": "string", "default": "crucible/konflux-watch-demo", "doc": "the fork branch every run reuses"},
}

konflux = tekton(
    cluster = "konflux",
    namespace = "open-data-hub-tenant",
    repository = "github-com-opendatahub-io-modelexpress",
    event = event.pull_request,
    expected = [
        pipeline_run("odh-modelexpress-on-pull-request", results = ["IMAGE_URL", "IMAGE_DIGEST"]),
        pipeline_run("odh-modelexpress-operator-on-pull-request", results = ["IMAGE_URL", "IMAGE_DIGEST"]),
    ],
)

pr = command(
    name = "pr",
    run = "python3 pr.py",
    emits = {"commit": "string", "number": "integer", "url": "link", "opened": "boolean"},
    timeout = "10m",
)

builds = watch(
    name = "builds",
    target = konflux,
    commit = pr.commit,
    timeout = "2h",
    appear = "10m",
)

report = command(
    name = "report",
    run = "python3 report.py",
    depends_on = [builds],
    emits = {"images": "list"},
)

workflow(type = "playbook", tasks = [pr, builds, report], result = report)

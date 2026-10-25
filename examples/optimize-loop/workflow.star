params = {
    "gpu": {
        "type": "bool",
        "default": False,
        "doc": "tune a PyTorch kernel on a GPU flavor instead of a Go program on fixed CPU",
    },
    "rounds": {
        "type": "int",
        "default": 3,
        "min": 1,
        "max": 5,
        "doc": "optimization rounds after the baseline",
    },
    "min_speedup": {
        "type": "number",
        "default": 5,
        "min": 1,
        "doc": "the speedup over the baseline the verifier requires",
    },
}

gpu = param("gpu")
harness = "gpu/bench.py" if gpu else "app/main.go"
kernel = "gpu/kernel.py" if gpu else "app/topk.go"

optimize = agent(
    name = "optimize",
    prompt = prompt_file("prompts/optimize-gpu.md" if gpu else "prompts/optimize.md") +
             "\n\nRound limit N: " + str(param("rounds")) +
             "\nThe verifier passes the run at a speedup of at least " +
             str(param("min_speedup")) + "x.\n",
    emits_files = {
        "report.json": schema_file("report.schema.json"),
        harness: None,
        kernel: None,
    },
    timeout = "100m",
)

# Checks the report's shape and nothing else, so the collector's prompt carries none of the
# optimizer's words.
gate = command(
    name = "gate",
    run = "python3 verify/verify.py gate",
    depends_on = [optimize],
)

collect = agent(
    name = "collect",
    prompt = prompt_file("prompts/collect.md"),
    model = "claude-sonnet-5",
    depends_on = [gate],
    emits_files = {"evidence/kueue.json": schema_file("evidence.schema.json")},
    timeout = "20m",
)

verify = evaluate(
    name = "verify",
    run = "python3 verify/verify.py verify",
    depends_on = [optimize, collect],
    emits = {"speedup": "number", "baseline": "object", "best": "object"},
)

workflow(type = "playbook", tasks = [optimize, gate, collect, verify], result = verify)

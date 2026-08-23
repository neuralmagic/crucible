params = {
    "paper_url": {
        "type": "string",
        "required": True,
        "doc": "arxiv abs/html URL, HuggingFace model page, or blog post",
        "pattern": "^https://(arxiv\\.org|huggingface\\.co)/",
    },
    "verifier": {
        "type": "string",
        "default": "Qwen/Qwen3-0.6B",
        "doc": "the verifier a smoke train would use",
    },
}

analyze = skill(
    name = "analyze",
    skill = "skills/analyze-paper",
    args = {"paper": param("paper_url"), "verifier": param("verifier")},
    emits = ["algo_name", "closest_model", "confidence"],
    emits_files = ["SPEC.md"],
)

# Deterministic, free, and the only thing standing between "the agent said it wrote a
# spec" and "there is a spec": the agent grades nothing here.
shape = command(
    name = "shape",
    run = "test -s SPEC.md && python3 -c \"import json,sys,pathlib; b=pathlib.Path('SPEC.md').read_text(); print(json.dumps({'bytes': len(b), 'has_classification': 'Classification' in b, 'has_training': 'Training' in b}))\"",
    depends_on = [analyze],
    emits = ["bytes"],
)

workflow(type = "playbook", tasks = [analyze, shape], result = shape)

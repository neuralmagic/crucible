params = {
    "codeword": {"type": "string", "required": True, "doc": "a random word only the first turn is told"},
}

memo = session(name = "memo")

job = agent(
    name = "job",
    prompt = prompt_file("prompts/job.md") + "\n\nCODEWORD: " + param("codeword") + "\n",
    session = memo,
    emits = {"job": "string", "status": ["succeeded", "failed", "timeout"]},
    repair = 1,
    timeout = "15m",
)

recall = agent(
    name = "recall",
    prompt = prompt_file("prompts/recall.md"),
    session = memo,
    depends_on = [job],
    emits = {"word": "string"},
    repair = 1,
    timeout = "5m",
)

fix = agent(
    name = "fix",
    prompt = prompt_file("prompts/fix.md"),
    depends_on = [recall],
    emits = {"count": "integer"},
    repair = 1,
    timeout = "5m",
)

verify = command(
    name = "verify",
    run = "python3 verify.py",
    depends_on = [job, recall, fix],
    emits = ["word_matches", "count_is_integer", "job_succeeded"],
)

workflow(type = "playbook", tasks = [job, recall, fix, verify], result = verify)

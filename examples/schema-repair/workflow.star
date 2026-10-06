probe = command(
    name = "probe",
    run = "printf '{\"ok\": true}' > PROBE.json && printf '{\"lanes\": [\"a\", \"b\"]}\n'",
    emits = {"lanes": schema_file("schemas/lanes.json")},
    emits_files = {"PROBE.json": schema_file("schemas/result.json")},
)

plan = agent(
    name = "plan",
    prompt = "Write PLAN.json and name the lanes to audit.",
    depends_on = [probe],
    emits = {"lanes": schema_file("schemas/lanes.json")},
    emits_files = {"PLAN.json": schema_file("schemas/result.json")},
    repair = 1,
)

audit = agent(
    name = "audit",
    prompt = "Audit one lane and write AUDIT.json.",
    depends_on = [plan],
    over = plan.lanes,
    max_fanout = 4,
    emits_files = {"AUDIT.json": schema_file("schemas/result.json")},
    repair = 1,
    required = False,
)

workflow(type = "playbook", tasks = [probe, plan, audit])

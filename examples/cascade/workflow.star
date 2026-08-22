# A cascade: one pass, no score. `type = "custom"` is what the engine admits today; the
# graph is already the cascade shape, so the lane costs this file one word.

scribe = session(name = "scribe")

draft = agent(
    name = "draft",
    prompt = prompt_file("prompts/draft.md"),
    session = scribe,
    emits = ["entries"],
)

shape = command(
    name = "shape",
    run = "./shape.sh",
    depends_on = [draft],
)

polish = agent(
    name = "polish",
    prompt = prompt_file("prompts/polish.md"),
    session = scribe,
    depends_on = [shape],
)

# topic, blocking
AUDITS = [
    ("headings", True),
    ("bullets", True),
    ("freshness", False),
]

def auditor(topic, blocking):
    return agent(
        name = "audit-" + topic,
        prompt = prompt_file("prompts/audit.md") + "\nAUDIT: " + topic.upper() + "\n",
        depends_on = [polish],
        isolated = True,
        required = blocking,
        emits = ["findings"],
    )

auditors = []
for topic, blocking in AUDITS:
    auditors.append(auditor(topic, blocking))

roundup = command(
    name = "roundup",
    run = "./roundup.sh",
    depends_on = deps(auditors),
    join = "passed",
)

workflow(
    type = "custom",
    tasks = [draft, shape, polish] + auditors + [roundup],
    result = roundup,
)

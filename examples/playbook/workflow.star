# A playbook: one pass, no score. No result task: the run's verdict is the graph's, not any
# single task's.

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
    depends_on = auditors,
    join = "passed",
)

# Engine-owned: publishes only the bounded run snapshot. Required means a delivery failure is a
# workflow failure; no agent can skip the call or choose its Slack payload.
publish_report = report(
    name = "publish-report",
    destination = {"kind": "slack"},
    template = "reports/slack.md.j2",
    required = True,
)

workflow(
    type = "playbook",
    tasks = [draft, shape, polish] + auditors + [roundup, publish_report],
)

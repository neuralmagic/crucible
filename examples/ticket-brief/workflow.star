params = {
    "jira_key": {
        "type": "string",
        "required": True,
        "pattern": "^[A-Z][A-Z0-9]*-[0-9]+$",
        "doc": "the ticket this run is about (PROJ-123); a watch passes each matching ticket's key here",
    },
    "note": {
        "type": "string",
        "default": "",
        "doc": "anything the launcher wants the brief to take into account",
    },
}

# The key reaches the agent through the prompt, the only place a parameter may go.
brief = skill(
    name = "brief",
    skill = "skills/brief-ticket",
    args = {"jira_key": param("jira_key"), "note": param("note")},
    emits = ["jira_key", "summary"],
    emits_files = ["BRIEF.md"],
)

# Free and deterministic: the report is stamped from what the brief captured.
report = command(
    name = "report",
    run = "python3 report.py",
    depends_on = [brief],
    emits = ["jira_key", "summary"],
    emits_files = ["REPORT.md"],
)

workflow(type = "playbook", tasks = [brief, report], result = report)

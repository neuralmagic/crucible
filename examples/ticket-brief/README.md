# ticket-brief

The smallest ticket-driven pack: a Jira key in, a one-page brief out. A `brief` agent turn
(one turn, a few cents) writes `BRIEF.md` for the key it was handed and emits the key
back; a free `report` command stamps `REPORT.md` from it. Nothing touches the network, so the
run costs the same whatever the ticket says.

It exists to be fired by every standing trigger:

- a manual launch or a one-shot, with `jira_key` typed in;
- a tracker watch, which passes each matching ticket's key as `jira_key` and launches once per
  ticket, ever.

```sh
crucible plan run --manifest examples/ticket-brief/crucible.toml \
  --param jira_key=INFERENG-10002 --max-cost 1 --max-time 10m
```

Registered on a controller, the same pack is `POST /api/playbooks/ticket-brief/launch` with
`{"params": {"jira_key": "INFERENG-10002"}}`, or a watch with `key_param = "jira_key"`.

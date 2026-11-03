# Architecture decision records

The ADRs capture the load-bearing design calls behind crucible: why the loop is
shaped the way it is, what was traded off, and what the alternatives were. They are
ordered, append-only, and meant to be read when you want the *why* rather than the *how*.

Each page is rendered by [govctl](https://github.com/govctl-org/govctl) from its source in
`gov/adr/`, and its header carries the record's status (proposed, accepted, superseded). Edit
the TOML source and run `govctl render adr`; CI fails when a page drifts from its source.
`govctl adr list` prints every record with its status.

Expand this section in the sidebar to browse the full list.

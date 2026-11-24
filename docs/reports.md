# Reports people read

A run ends by telling someone something: which item is blocked, what changed, whether the release
is good to go. That report is the product. Most packs under-invest in it, and a generated wall of
text is the usual result: every fact is present, the reader still cannot act.

Rules that survive contact with a busy reader:

**Lead with the answer.** The first two sentences say what happened and whether it is good news.
A reader who stops after those two sentences should not be misled.

```text
Release 3.5.2 is blocked: the pipeline MR is still open and the newest image predates it.
```

not

```text
This report summarizes the current state of the 3.5.2 release across several systems, including
pipelines, images, and the source tag, with the following observations...
```

**One screen, then detail.** Aim for what / so what / what next, in that order. Longer material
goes below the fold or into a linked file, not into the middle of the argument.

**Link evidence, do not recite it.** A digest, an MR number and a scan URL are worth more than a
paragraph describing them, and they are what the reader will need when the summary is wrong.
Every claim a person might check should be one click from the claim.

**Do not narrate the diff.** "It is X, not Y", "changed from A to B", "note that previously" —
a reader who did not see the previous version does not need its history. State the current fact.
This is the single most common way a generated report becomes unreadable.

**Say what you could not verify.** An unknown named as unknown is a finding. An unknown smoothed
into confident prose is a lie with good formatting. A pack should report an unreadable API as
`unknown`, never as `zero`.

**Separate the reader from the parser.** If an agent will consume the report too, keep the full
detail: put it in a fenced block, an appendix section, or a linked artifact, and keep the top of
the report for people. One artifact can serve both, in that order.

## A shape to start from

```markdown
# <Release / item> — <status in one word>

<One or two sentences: the outcome, and the one fact that decides it.>

## What changed
- <fact with a link>
- <fact with a link>

## What is next
- <owner>: <action> — <date or trigger>

## Evidence
- <links, digests, ids>

## Unknowns
- <what could not be read, and what would resolve it>
```

Steal the shape, not the wording. If a house style exists — a technical English standard, a team
template, a reviewed report from last month — hand it to the agent as the example to match. An
example is a better instruction than any adjective: it fixes length, density and voice at once.

## If a model wrote it

Read it before you send it. Two failures are cheap to catch and expensive to live down: a summary
that states the opposite of its own evidence, and a summary that repeats one fact in three
sections because the draft grew a paragraph at a time. Both are obvious to the person who wrote
the run and invisible to the model that wrote the report.

A report that misleads costs more than no report, because the next one gets read with suspicion.

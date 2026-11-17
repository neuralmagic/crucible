# View components

A view is a title and one component. A component is a JSON object with one key, its name, and
its fields under it. Rust builds the same values as `crucible_view::Node` variants. Every `tone`
is optional and is one of `neutral`, `muted`, `good`, `warn`, `bad`, `info`: a colour role
from the app's theme, the same in light and dark.

## Layout

`stack` puts children top to bottom. `tabs` shows one body at a time; a tab's `count` is drawn
beside its label.

```json
{"stack": {"children": [
  {"text": {"text": "Triaged from two lanes.", "tone": "muted"}},
  {"tabs": {"id": "by", "tabs": [
    {"label": "Summary", "body": {"text": {"text": "..."}}},
    {"label": "All", "count": 64, "body": {"text": {"text": "..."}}}
  ]}}
]}}
```

## Numbers

`stats` is a row of headline numbers. A `value` is text, so format it yourself.

```json
{"stats": {"items": [
  {"label": "Triaged", "value": "64"},
  {"label": "Affected", "value": "8", "tone": "bad"}
]}}
```

`grid` is a small table drawn whole, for a pivot. The first column is the label and the rest
are right-aligned. At most 600 cells, header included.

```json
{"grid": {
  "columns": ["category", "total", "New"],
  "rows": [
    [{"text": "affected", "tone": "bad"}, {"text": "8"}, {"text": "4"}],
    [{"text": "total"}, {"text": "64"}, {"text": "40"}]
  ]
}}
```

## Table

`table` holds any number of rows. The reader sorts it by a click on a column header and
filters it with the box above it, which keeps the rows holding every word typed, in any cell.
A cell with `href` is a link (see Links), and a double click on a row opens its first link.

A column's `kind` is `text`, `mono`, `badge` (a dot in the cell's tone before the text) or
`number` (right-aligned); its `width` is `narrow`, `medium`, `wide` or `fill`. Both are
optional.

Sorting is by the cell text in natural order, so `item2` comes before `item10`. A `number`
column sorts text that is a number, such as `6.1` or `1,200`, as that number. For anything
shown with a unit, give the cell a `value`: `{"text": "3 min", "value": 180}` sorts by 180 in
any kind of column. Use it for durations, sizes, dates (as a timestamp) and percentages. Cells
with no number sort after those with one.

A row's optional `detail` is Markdown shown in a panel under the table while that row is
selected. Put there what does not fit a cell: the whole rationale, the evidence, links to the
task and file it came from. Keep the cells to what a reader scans and sorts by.

```json
{"table": {
  "id": "all",
  "columns": [
    {"id": "ticket", "title": "Ticket", "kind": "mono", "width": "narrow"},
    {"id": "category", "title": "Category", "kind": "badge"},
    {"id": "open", "title": "Open", "kind": "number", "width": "narrow"},
    {"id": "why", "title": "Why", "width": "fill"}
  ],
  "rows": [
    {"id": "DEMO-101", "cells": [
      {"text": "DEMO-101", "href": "https://issues.example.com/browse/DEMO-101"},
      {"text": "affected", "tone": "bad"},
      {"text": "57 d", "value": 57},
      {"text": "The handler is reachable from the public listener."}
    ], "detail": "### DEMO-101\n\nThe handler is reachable from the public listener.\n\nTriaged by [triage-go](run:task/triage-go), from [its verdicts](run:file/triage-go/verdicts.json)."}
  ]
}}
```

Rules the app checks:

- Every row has one cell per column, and a row `id` is unique in its table.
- A table `id` and a tabs `id` are unique in the view.
- Only one table is on screen at a time. Put each table in its own tab; a stack may not hold
  two, and may not hold a table beside tabs that hold one.
- An `href` is one of the addresses under Links.
- All tables together hold at most 500,000 cells, and a row's `detail` at most 100,000 bytes.

## Text

`text` is one plain paragraph in a tone. `markdown` is Markdown: headings, lists, tables, code
blocks, links, and diagrams in `mermaid` fences. Images are not drawn.

```json
{"markdown": {"text": "## Reachability\n\n- **8 affected**, 3 of them on the public listener\n- see [triage-go](run:task/triage-go)\n\n`grpc 1.64.0` is in every image."}}
```

`mermaid` is one diagram from its Mermaid source: a flowchart, a sequence, state, class or
entity-relationship diagram, a gantt chart, a pie, a mindmap, a timeline, and others. A source
the app cannot read is shown as its text. Reach for it for structure and process (how a
verdict was reached, what calls what); use `plot`, `bars` and `flow` for quantities.

```json
{"mermaid": {"source": "flowchart LR\n  ticket([Ticket]) --> shipped{Shipped?}\n  shipped -- no --> not_shipped\n  shipped -- yes --> verdict{Verdict}\n  verdict -- affected --> affected\n  verdict -- not affected --> not_affected"}}
```

All `markdown` and `mermaid` in a view together hold at most 100,000 bytes.

## Links

An address, in a cell's `href` or a Markdown link, is one of:

| Address | Leads to |
| --- | --- |
| `https://...` or `http://...` | the web page, in the browser |
| `run:task/<task>` | that task of the run, in the Tasks pane; `task[key]` is one instance of a fan-out, in the Data pane |
| `run:file/<task>/<path>` | that captured file, in the Files pane |

`<task>` and `<path>` are a file's `task` and `path` exactly as the request gives them, so the
address of a file the plugin read is `run:file/` + `task` + `/` + `path`. In Rust,
`crucible_view::task_link` and `file_link` build them. Any other address in a cell is refused;
in Markdown it is drawn and leads nowhere. A cell whose task or file this run does not have is
drawn as plain text. A local run shows tasks but lists no captured files, so `run:file` links
lead somewhere only for a run on a controller.

## Charts

`bars` are horizontal bars on one scale, each split into segments. Segments with the same
`label` share a legend entry. Values are finite and not negative.

```json
{"bars": {"bars": [
  {"label": "3.4", "segments": [
    {"label": "affected", "value": 3, "tone": "bad"},
    {"label": "not_affected", "value": 7, "tone": "good"}
  ]},
  {"label": "3.5", "segments": [
    {"label": "affected", "value": 1, "tone": "bad"},
    {"label": "not_affected", "value": 9, "tone": "good"}
  ]}
]}}
```

`plot` is points on shared axes: `line` joins each series in the order given, `scatter` leaves
marks. `x` and `y` name what the axes measure. A point is `[x, y]`, both finite. The vertical
axis starts at zero unless a value is below it; the horizontal runs from the least to the
greatest x.

```json
{"plot": {
  "kind": "scatter",
  "x": "CVSS score",
  "y": "days open",
  "series": [
    {"label": "affected", "tone": "bad", "points": [[7.5, 12], [9.1, 40]]},
    {"label": "needs_review", "tone": "warn", "points": [[5.0, 3]]}
  ]
}}
```

A series is coloured by its tone, and there are six tones, so two series with one tone look the
same. Past five or six series, split the chart or use a table. A bar segment's value is from 0
to 1e15, and a point's x and y are within 1e15 of zero.

`flow` is quantities moving between stages, drawn as ribbons: a Sankey diagram. Use it to show
how a total splits and where each part ended up. A node is a stage and a link is a ribbon as
wide as its `value`. The app puts a stage with nothing flowing into it in the first column and
every other stage one column past the furthest stage that flows into it; stages in a column
are stacked in the order given. A ribbon takes the tone of the stage it reaches unless it has
its own.

```json
{"flow": {
  "nodes": [
    {"id": "go", "label": "Go", "tone": "muted"},
    {"id": "python", "label": "Python", "tone": "muted"},
    {"id": "reachability", "label": "reachability"},
    {"id": "scope", "label": "scope"},
    {"id": "affected", "label": "affected", "tone": "bad"},
    {"id": "cleared", "label": "not affected", "tone": "good"}
  ],
  "links": [
    {"from": "go", "to": "reachability", "value": 22},
    {"from": "go", "to": "scope", "value": 12},
    {"from": "python", "to": "reachability", "value": 18},
    {"from": "python", "to": "scope", "value": 12},
    {"from": "reachability", "to": "affected", "value": 8},
    {"from": "reachability", "to": "cleared", "value": 32},
    {"from": "scope", "to": "cleared", "value": 24}
  ]
}}
```

A node `id` is unique in its flow, a link names two of them, a `value` is greater than zero,
and no chain of links leads back to a stage it left. At most 60 stages and 400 links. Keep
labels short: the first and last columns have about 120 points for theirs.

## Canvas

`canvas` is free drawing with the commands of the HTML canvas's 2D context, under the same
names in snake case. `width` and `height` are points, 1 to 4,000; coordinates start at the top
left with y down; angles are radians and arcs run clockwise. A command with no arguments is its
name as a string. Every number is within 1,000,000 of zero.

A path may run 1,000 points before its next `move_to`, `close_path` or `rect`. A line adds one
point, a curve up to 65, an arc up to 49 and a `rect` 5. Every `fill` and `stroke` counts all
the points of the path it draws, so call `begin_path` before each new shape.

```json
{"canvas": {"width": 240, "height": 120, "commands": [
  {"fill_style": "bad"},
  {"global_alpha": 0.6},
  {"fill_rect": [10, 10, 60, 24]},
  {"global_alpha": 1},
  {"stroke_style": "#3b82f6"},
  {"line_width": 8},
  {"line_cap": "round"},
  "begin_path",
  {"arc": {"x": 170, "y": 60, "radius": 40, "start_angle": -1.5708, "end_angle": 2.8}},
  "stroke",
  {"fill_style": "neutral"},
  {"font": "bold 16px monospace"},
  {"text_align": "center"},
  {"fill_text": {"text": "70%", "x": 170, "y": 65}}
]}}
```

| Command | Arguments |
| --- | --- |
| `fill_style`, `stroke_style` | a tone name, or `#rgb`, `#rrggbb`, `#rrggbbaa` |
| `global_alpha`, `line_width` | a number |
| `line_cap` | `butt` or `round` |
| `font` | a CSS font string, read for its `px` size and the words `bold` and `monospace` |
| `text_align` | `start`, `left`, `center`, `right` or `end` |
| `save`, `restore` | none; they keep and bring back the styles above |
| `begin_path`, `close_path`, `fill`, `stroke` | none |
| `move_to`, `line_to` | `[x, y]` |
| `quadratic_curve_to` | `[cpx, cpy, x, y]` |
| `bezier_curve_to` | `[cp1x, cp1y, cp2x, cp2y, x, y]` |
| `arc` | `x`, `y`, `radius`, `start_angle`, `end_angle`, optional `counterclockwise` |
| `rect`, `fill_rect`, `stroke_rect` | `[x, y, width, height]` |
| `fill_text` | `text`, `x`, `y`, with y at the text's baseline |

In Rust, `crucible_view::Context2d` records the same commands with the method names `web-sys`
uses (`set_fill_style`, `begin_path`, `fill_rect`, `fill_text`), and `finish()` returns the
component.

What a browser's canvas has and this does not: transforms (`translate`, `scale`, `rotate`),
clipping, images, gradients, patterns, shadows, `stroke_text`, `measure_text`, `clear_rect`.
Text is always drawn over shapes, whatever the order of the commands. A canvas is drawn at its
own size and is cut off where the pane is narrower, so keep it under about 600 points wide.
Prefer a tone to a hex colour: a hex colour does not follow the theme.

## Whole-view limits

Across the whole view, at most:

- 2,000 components, nested at most 8 deep;
- 20,000 stats, bars, segments, series, grid cells, table columns, and flow stages and links;
- 50,000 points, counting plot points, the points of each canvas path filled or stroked, and 4
  for each `fill_rect` or `stroke_rect`;
- 20,000 canvas commands;
- 20,000 bytes in any one title, label, cell or address.

A view past any limit on this page is refused whole, with the limit named in the pane.

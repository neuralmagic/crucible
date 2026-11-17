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

`columns` puts its children side by side at equal widths, and wraps them onto further lines
when the pane is narrower than about 320 points for each, or than the widest `canvas` or `flow`
a child holds. Use it for two or three charts that read together. A child is as tall as its
content, and `columns` may not hold a table, however deep.

```json
{"columns": {"children": [
  {"stats": {"items": [{"label": "Affected", "value": "8", "tone": "bad"}]}},
  {"stats": {"items": [{"label": "Closable", "value": "48", "tone": "good"}]}}
]}}
```

`section` is a `body` under a `title` the reader clicks to fold and unfold. It starts unfolded
unless `open` is `false`; fold what most readers skip. The app keeps what the reader folded
across refreshes of the same run. A section's `id` is unique in the view, like a tab's.

```json
{"section": {"id": "how", "title": "How a ticket is categorised", "open": false,
  "body": {"markdown": {"text": "A ticket not in the final image is `sbom_inaccurate`."}}}}
```

## Numbers

`stats` is a row of headline numbers. A `value` is text, so format it yourself.

```json
{"stats": {"items": [
  {"label": "Triaged", "value": "64"},
  {"label": "Affected", "value": "8", "tone": "bad"}
]}}
```

`meter` is one value against a maximum, drawn as a bar in its tone under its label and both
figures. `max` is above 0 and at most 1e15, `value` is within 1e15 of zero, and the bar is drawn
empty at 0 or below and full at `max` or above. Put several in `columns`.

```json
{"meter": {"label": "Closable", "value": 45, "max": 64, "tone": "good"}}
```

`grid` is a small table drawn whole, for a pivot. The first column is the label and the rest
are right-aligned. At most 600 cells, header included. A grid draws no links and does not sort,
so it ignores a cell's `href` and `value`, though it checks them as a table does.

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
The button beside the box copies the rows as filtered and sorted, as CSV under a line of the
column titles; a cell that would read as a spreadsheet formula starts with an apostrophe.
A cell with `href` is a link (see Links), and a double click on a row opens its first link.

A column's `kind` is `text`, `mono`, `badge` (a dot in the cell's tone before the text) or
`number` (right-aligned); its `width` is `narrow`, `medium`, `wide` or `fill`. Both are
optional.

Sorting is by the cell text in natural order, so `item2` comes before `item10`. A `number`
column sorts text that is a number, such as `6.1` or `1,200`, as that number. For anything
shown with a unit, give the cell a `value`: `{"text": "3 min", "value": 180}` sorts by 180 in
any kind of column. Use it for durations, sizes, dates (as a timestamp) and percentages. Cells
with no number sort after those with one.

Rather than formatting a number by hand, build the cell with a helper that sets both its text
and its `value`: `Cell.duration(seconds)` in a script or `Cell::duration(seconds)` in Rust, and
likewise `bytes`, `percent` (from a fraction), `count` and `date` (from Unix seconds). Both give
the same text:

| Helper | Input | Text |
| --- | --- | --- |
| `duration` | `0.25`, `4.5`, `200`, `7500`, `273600` | `250 ms`, `4.5 s`, `3 min 20 s`, `2 h 5 min`, `3 d 4 h` |
| `bytes` | `512`, `1536`, `3435973837` | `512 B`, `1.5 KiB`, `3.2 GiB` |
| `percent` | `0.453`, `1` | `45.3%`, `100%` |
| `count` | `1234567` | `1,234,567` |
| `date` | `1793491200` | `2026-11-01`, in UTC |

A number that is not finite or is beyond 1e15 of zero, or a date outside the years 1 to 9998,
gives an empty cell.

A cell's optional `bar`, from 0 to 1, draws a thin bar under its text in the cell's tone, as
long as that share of the cell: use it for a column of shares or scores.

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
    {"id": "cvss", "title": "CVSS", "kind": "number", "width": "narrow"},
    {"id": "why", "title": "Why", "width": "fill"}
  ],
  "rows": [
    {"id": "DEMO-101", "cells": [
      {"text": "DEMO-101", "href": "https://issues.example.com/browse/DEMO-101"},
      {"text": "affected", "tone": "bad"},
      {"text": "57 d", "value": 4924800},
      {"text": "9.1", "value": 9.1, "tone": "bad", "bar": 0.91},
      {"text": "The handler is reachable from the public listener."}
    ], "detail": "### DEMO-101\n\nThe handler is reachable from the public listener.\n\nTriaged by [triage-go](run:task/triage-go), from [its verdicts](run:file/triage-go/verdicts.json)."}
  ]
}}
```

Rules the app checks:

- Every row has one cell per column, and a row `id` is unique in its table.
- A table `id`, a tabs `id` and a section `id` are unique in the view.
- Only one table is on screen at a time. Put each table in its own tab; a stack may not hold
  two, and may not hold a table beside tabs that hold one. A folded section's table counts,
  since the reader may unfold it.
- `columns` hold no table.
- An `href` is one of the addresses under Links.
- A cell's `value` is within 1e15 of zero, and its `bar` is from 0 to 1.
- All tables together hold at most 500,000 cells, and a row's `detail` at most 100,000 bytes.

## Text

`text` is one plain paragraph in a tone. `markdown` is Markdown: headings, lists, tables, code
blocks, links, and diagrams in `mermaid` fences. Images are not drawn.

```json
{"markdown": {"text": "## Reachability\n\n- **8 affected**, 3 of them on the public listener\n- see [triage-go](run:task/triage-go)\n\n`grpc 1.64.0` is in every image."}}
```

`note` says there is nothing to show yet, or that what is shown is partial: a `headline`, an
optional `detail` and a `tone`, drawn in a card like the pane's own notes. Return it in place of
the body, or above a part, rather than an error, so the reader keeps the controls.

```json
{"note": {"headline": "No ticket matches.", "detail": "Lower Min CVSS or pick another lane.", "tone": "warn"}}
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
| `launch:<id>` | that launch of the playbook, by a launch's `id` in a runs request |
| `launch:<id>/task/<task>` | that launch, on that task |

`<task>` and `<path>` are a file's `task` and `path` exactly as the request gives them, so the
address of a file the plugin read is `run:file/` + `task` + `/` + `path`. In Rust,
`crucible_view::task_link`, `file_link`, `launch_link` and `launch_task_link` build them. Any
other address in a cell is refused; in Markdown it is drawn and leads nowhere. A cell whose task,
file or launch the app does not have is drawn as plain text. A local run shows tasks but lists no
captured files, so `run:file` links lead somewhere only for a run on a controller, and `launch:`
links only on a controller.

## Runs view

A runs view is built from the launches of the playbook rather than one run (see `SKILL.md`), and
drawn on the playbook's runs page with every component on this page. It has no one run, so a
cell's `run:task` or `run:file` address is refused there, and one in Markdown leads nowhere;
link each launch with `launch:` instead. A row per launch, linked, beside a chart of how a count
moved:

```json runs
{"stack": {"children": [
  {"bars": {"bars": [
    {"label": "triage:11", "segments": [{"label": "affected", "value": 8, "tone": "bad"}]},
    {"label": "triage:12", "segments": [{"label": "affected", "value": 9, "tone": "bad"}]}
  ]}},
  {"table": {"id": "launches", "columns": [
    {"id": "launch", "title": "Launch", "kind": "mono"},
    {"id": "affected", "title": "Affected", "kind": "number"}
  ], "rows": [
    {"id": "triage:12", "cells": [{"text": "triage:12", "href": "launch:triage:12"}, {"text": "9"}]},
    {"id": "triage:11", "cells": [{"text": "triage:11", "href": "launch:triage:11/task/triage-go"}, {"text": "8"}]}
  ]}}
]}}
```

## Charts

`bars` are horizontal bars on one scale, each split into segments. Segments with the same
`label` share a legend entry and a colour. Values are finite and not negative.

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
marks. `x` and `y` name what the axes measure. A point is `[x, y]`, both finite. Each axis is
ticked at round numbers (1, 2 or 5 times a power of ten) with a grid line on each tick. A
linear vertical axis includes zero; the horizontal runs from about the least to the greatest x.

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

`x_scale` and `y_scale` are `linear` (the default), `log` or `time`. On a `log` axis every
value is above zero and the ticks are powers of ten. On a `time` axis a value is Unix seconds
from the start of the year 1 to the start of 9999, and the ticks are dates or times in UTC at
steps that suit the span: seconds, minutes, hours, days, weeks, months or years.

```json
{"plot": {
  "kind": "line",
  "x": "opened",
  "y": "tickets",
  "x_scale": "time",
  "y_scale": "log",
  "series": [{"label": "go", "points": [[1788566400, 1], [1789776000, 4], [1791504000, 30]]}]
}}
```

`categories` names the x axis's places: an x is the index of a name, from 0, and the ticks are
the names, at most 12 of them written when there are more. An x that is not an index of a name
is refused, and so is a plot with categories on a `log` or `time` x axis.

```json
{"plot": {
  "kind": "line",
  "x": "release",
  "y": "tickets",
  "categories": ["3.3", "3.4", "3.5"],
  "series": [{"label": "affected", "tone": "bad", "points": [[0, 2], [1, 3], [2, 1]]}]
}}
```

While a plot holds at most 1,000 points, hovering one shows its series and its x and y.

`histogram` counts `values` into bins of equal width from the least value to the greatest and
draws a bar per bin on a count axis. `bins` is from 1 to 100; without it the app takes the
square root of the number of values, at most 30. Hovering a bar shows its range and count.

```json
{"histogram": {"x": "days open", "values": [18, 21, 25, 40, 44, 57, 57, 90], "bins": 4}}
```

`heatmap` is a matrix: `columns` labels its columns, `rows` its rows, and `values` holds a row
per row label with a value per column label. Each cell is filled in the one `tone` as deep as
its value's place between the least value and the greatest, and its value is written in it
while there are at most 16 columns. Every row has one value per column. While it holds at most
2,000 cells, hovering one shows its row, column and value.

```json
{"heatmap": {
  "columns": ["3.3", "3.4", "3.5"],
  "rows": ["affected", "not_affected"],
  "values": [[2, 3, 1], [10, 14, 9]],
  "tone": "info"
}}
```

A series, bar segment or flow stage with a tone is drawn in that tone's colour. One left
`neutral` (or without a tone) takes the next colour of a palette of ten the app keeps distinct
in both themes, the same colour in the marks and the legend. Use tones for what they mean
(`bad` is bad) and leave the rest neutral. A histogram or heatmap left neutral takes the
palette's first colour. A bar segment's value is from 0 to 1e15, and a point's x and y, a
histogram's values and a heatmap's values are within 1e15 of zero.

`flow` is quantities moving between stages, drawn as ribbons: a Sankey diagram. Use it to show
how a total splits and where each part ended up. A node is a stage and a link is a ribbon as
wide as its `value`. The app puts a stage with nothing flowing into it in the first column and
every other stage one column past the furthest stage that flows into it; stages in a column
are stacked in the order given. A ribbon takes the colour of the stage it reaches unless it has
a tone of its own other than `neutral`.

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
left with y down; angles are radians and an arc runs clockwise unless it sets
`counterclockwise`. A command with no arguments is its name as a string. Every number is within
1,000,000 of zero.

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
  {"text_baseline": "middle"},
  {"fill_text": {"text": "70%", "x": 170, "y": 60}},
  {"hint": {"x": 130, "y": 20, "width": 80, "height": 80, "text": "7 of 10 closable"}},
  "save",
  {"translate": [40, 70]},
  {"rotate": -0.3},
  {"fill_style": "muted"},
  {"fill_rect": [-30, -8, 60, 16]},
  "restore",
  {"text_align": "left"},
  {"font": "11px sans-serif"},
  {"fill_text": {"text": "a label cut to fit", "x": 10, "y": 105, "max_width": 70}}
]}}
```

| Command | Arguments |
| --- | --- |
| `fill_style`, `stroke_style` | a tone name, or `#rgb`, `#rrggbb`, `#rrggbbaa` |
| `global_alpha`, `line_width` | a number |
| `line_cap` | `butt` or `round`; a script's `square` is drawn as `butt` |
| `font` | a CSS font string, read for its `px` size and the words `bold` and `monospace` |
| `text_align` | `start`, `left`, `center`, `right` or `end` |
| `text_baseline` | `top`, `middle`, `alphabetic` or `bottom` |
| `save`, `restore` | none; they keep and bring back the styles above and the transform |
| `translate`, `scale` | `[x, y]` |
| `rotate` | an angle in radians, clockwise |
| `transform`, `set_transform` | `[a, b, c, d, e, f]`, as the 2D context takes them |
| `reset_transform` | none |
| `begin_path`, `close_path`, `fill`, `stroke` | none |
| `move_to`, `line_to` | `[x, y]` |
| `quadratic_curve_to` | `[cpx, cpy, x, y]` |
| `bezier_curve_to` | `[cp1x, cp1y, cp2x, cp2y, x, y]` |
| `arc` | `x`, `y`, `radius`, `start_angle`, `end_angle`, optional `counterclockwise` |
| `rect`, `fill_rect`, `stroke_rect` | `[x, y, width, height]` |
| `fill_text` | `text`, `x`, `y`, optional `max_width` |
| `hint` | `x`, `y`, `width`, `height`, `text`: shown on hover over the rectangle |
| `hit` | `id`, `x`, `y`, `width`, `height`: a rectangle the reader clicks; see "Live views" |

A transform applies to every point as it is added to the path, to `fill_rect`, `stroke_rect`,
`hint` and `hit` rectangles, to line widths, and to where text is drawn. Text is never rotated or
skewed: it is drawn upright at its transformed point, its size scaled by the transform. Text
longer than `max_width` ends in an ellipsis; a `max_width` of 0 or less draws nothing. A `hint`
is not the 2D context's: its text shows on hover over its rectangle, and each counts as an item.
Shapes and text are painted in the order of their commands.

In Rust, `crucible_view::Context2d` records the same commands with the method names `web-sys`
uses (`set_fill_style`, `begin_path`, `fill_rect`, `arc`, `arc_with_anticlockwise`, `fill_text`,
`fill_text_with_max_width`, `set_transform`), and `finish()` returns the component. In a
script, `ctx.hint(x, y, width, height, text)` adds a hint and `ctx.hit(x, y, width, height, id)`
a hit, as `hint` and `hit` do in Rust.

What a browser's canvas has and this does not: clipping, holes in a fill (the `evenodd` rule;
each subpath fills on its own), images, gradients, patterns, shadows, `stroke_text`,
`measure_text`, `clear_rect`. A canvas wider than the pane is shrunk to fit it, text and all.
Prefer a tone to a hex colour: a hex colour does not follow the theme.

## Controls

A view's `controls` are drawn in a row above its body: choices the reader makes that you
would otherwise precompute as tabs. When the reader changes one, the app runs the plugin again
with every control's value in the request's `params` and draws the new view in place of the
old. Until the reader changes one, `params` is empty, so every control's `value` (or `on`) is
its default. Return each control with the value you were given so it stays where the reader put
it. The app sends only values the last view's controls offer: a select's value is one of its
options, a range's lies on a step from `min`, or at `max`.

`controls` sits beside `title` and `body`, and is left out when there are none. Each control is
an object with one key, its kind:

- `select`: one of `options`, each a `value` and a `label`; `value` is one of the options'.
  At most 100 options, their values and labels each unique.
- `toggle`: a switch, `on` or off.
- `range`: a number from `min` to `max` in steps of `step`, set when the reader lets go of
  it. `min` is below `max`, `step` is above 0, there are at most 1,000 steps from one end to
  the other, and `value` lies between them.

```json controls
[
  {"select": {"id": "lane", "label": "Lane", "value": "all", "options": [
    {"value": "all", "label": "All"},
    {"value": "go", "label": "Go"}
  ]}},
  {"toggle": {"id": "open", "label": "Open only", "on": false}},
  {"range": {"id": "cvss", "label": "Min CVSS", "min": 0, "max": 10, "step": 0.5, "value": 0}}
]
```

A script reads `params` as plain values (`params.lane` is `"go"`, `params.cvss` is `7.5`); the
request carries each under its kind (`{"lane": {"select": "go"}}`), which a Rust plugin reads
with `request.select("lane")`, `request.toggle("open")` and `request.range("cvss")`. At most 12
controls, their ids unique among themselves.

## Live views

A view is live when it returns `live` beside `title` and `body`. The app then runs the plugin
again on an event and draws each new view in place of the last: a game, a stepper, a view that
pages through its data. The plugin has no clock and no memory of its own; everything it needs
from one run to the next goes in `state`, and the next request hands it back.

- `state`: text, at most 65,536 bytes, handed back as the request's `state`. Until a view
  returns one, `state` is `null` (`None` in Rust).
- `tick_ms`: run again with the event `tick` this many milliseconds on, from 50 to 3,600,000,
  while the view is on screen. Leave it out to stop the clock.
- `keys`: the keys that run it again while the view has focus, at most 6 of `left`, `right`,
  `up`, `down`, `space` and `enter`. Other keys stay the app's.

```json live
{"title": "Counter",
 "body": {"canvas": {"width": 200, "height": 40, "commands": [
   {"fill_style": "muted"},
   {"fill_rect": [0, 0, 60, 40]},
   {"fill_style": "neutral"},
   {"text_align": "center"},
   {"fill_text": {"text": "+1", "x": 30, "y": 25}},
   {"hit": {"id": "add", "x": 0, "y": 0, "width": 60, "height": 40}},
   {"text_align": "left"},
   {"fill_text": {"text": "3", "x": 80, "y": 25}}
 ]}},
 "live": {"state": "3", "tick_ms": 1000, "keys": ["up", "down"]}}
```

The request's `event` says what ran it: a click on a canvas `hit` rectangle by its `id`, a key
the view asked for, or the tick. It is `null` (`None`) when the files or the controls ran it.

```json events
[{"hit": "add"}, {"key": "up"}, "tick"]
```

A `hit` is a canvas command that draws nothing: its rectangle, under the current transform,
takes clicks above whatever the canvas draws there. Each counts as an item, like a `hint`, and
its `id` is at most as long as a label.

The app answers events in order. While one is unanswered, later clicks and keys wait, at most 8
of them, and a tick that comes due is dropped, so a slow plugin is never asked faster than it
answers. Each request carries the state the newest view returned. Picking another pack, run or
playbook starts over with no state. Controls and live views mix: a control change runs the
plugin with the current state and no event.

## Whole-view limits

Across the whole view, at most:

- 2,000 components, nested at most 8 deep;
- 20,000 stats, meters, bars, segments, series, plot categories, grid cells, heatmap cells and
  labels, table columns, flow stages and links, and canvas hints and hits;
- 50,000 points, counting plot points, histogram values, the points of each canvas path filled
  or stroked, and 4 for each `fill_rect` or `stroke_rect`;
- 20,000 canvas commands;
- 20,000 bytes in any one title, label, cell or address.

A view past any limit on this page is refused whole, with the limit named in the pane.

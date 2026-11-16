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

`table` holds any number of rows and sorts by a click on a column header, in natural order of
the cell text. A cell with `href` is a link, and a double click on a row opens its first link.
A column's `kind` is `text`, `mono` or `badge` (a dot in the cell's tone before the text); its
`width` is `narrow`, `medium`, `wide` or `fill`. Both are optional.

```json
{"table": {
  "id": "all",
  "columns": [
    {"id": "ticket", "title": "Ticket", "kind": "mono", "width": "narrow"},
    {"id": "category", "title": "Category", "kind": "badge"},
    {"id": "why", "title": "Why", "width": "fill"}
  ],
  "rows": [
    {"id": "DEMO-101", "cells": [
      {"text": "DEMO-101", "href": "https://issues.example.com/browse/DEMO-101"},
      {"text": "affected", "tone": "bad"},
      {"text": "The handler is reachable from the public listener."}
    ]}
  ]
}}
```

Rules the app checks:

- Every row has one cell per column, and a row `id` is unique in its table.
- A table `id` and a tabs `id` are unique in the view.
- Only one table is on screen at a time. Put each table in its own tab; a stack may not hold
  two, and may not hold a table beside tabs that hold one.
- An `href` starts with `http://` or `https://`.
- All tables together hold at most 500,000 cells.

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
same. Past five or six series, split the chart or use a table. At most 1,000 segments in one
set of bars and 20,000 points in one plot.

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
name as a string. At most 20,000 commands.

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

At most 2,000 components nested at most 8 deep. A view past any limit on this page is refused
whole, with the limit named in the pane.

---
name: write-a-view-plugin
description: Give a crucible playbook pack its own view of a run (a report, dashboard, table, pivot, chart or drawing in the desktop app's Report pane), and optionally of all its launches on the playbook's runs page, by writing a view plugin in the pack's ui/ folder, in TypeScript or JavaScript, or in Rust compiled to WebAssembly. Use when asked to add or change a pack's report, custom view, chart, table or dashboard, to show how results changed across launches, or to turn a run's captured files into something a person reads.
---

# Writing a view plugin

A view plugin is a file in a pack's `ui/` folder: JavaScript source, or a WebAssembly module.
The desktop app hands it the files a run's tasks captured and draws what it returns in the
Report pane. The plugin computes and the app draws: you return a tree of components from a
fixed set, never pixels or HTML.

Two working packs to copy from: `examples/report-ts` has its plugin in TypeScript
(`ui/report.ts`), `examples/report` in Rust (`ui/report/`). `components.md` beside this file
lists every component with its JSON.

Write it in TypeScript unless the pack already has a Rust plugin. A script is text a reviewer
can read, needs no build beyond a type check, and checks in under a second. Rust gives a
smaller, faster plugin and is worth it for a view that shapes a lot of data.

## What a plugin can and cannot do

- It sees only the captured files it names, as text. A file over 8 MiB or not UTF-8 is refused,
  and so are files that together hold more than 32 MiB.
- It has no network, filesystem, arguments or environment. Its clock counts its own readings
  and its random bytes never change, so the view must depend on the files alone, and for a
  live view on the state and event the app hands it.
- It runs under limits: about two seconds of work, 256 MiB of memory, a 64 MiB reply, 16 input
  paths. Past one, the pane shows the error instead of the view.
- A view the app cannot check is refused whole: an unknown component or field, a row whose
  cells do not match its columns, a number out of range, more than one table on screen.

## The loop

1. **Find the data.** The plugin reads files tasks declare in `emits_files`, by the path the
   task declared (`verdicts.json`, `evidence/probe.json`). If the facts you need are only in
   task output or logs, add a task that writes them to a file first. Prefer one JSON file with a
   schema (`schema_file(...)`) over scraping text.
2. **Shape it in the plugin.** Every task that captured a named path gives you one file, so a
   fan-out or a set of lanes arrives as several files to merge. Decide the row (one per ticket,
   per finding, per benchmark), dedupe, sort, count. Keep that logic here, not in a script
   someone runs after the run.
3. **Choose components.** Read `components.md`. Headline numbers are `stats`, a part of a
   whole is a `meter`, a pivot is a `grid`, the rows are a `table`, a category split is
   `bars`, an ordered x axis is a `line`
   plot (on a `time` axis for dates, with `categories` for named steps), two measures are a
   `scatter` plot, one measure's spread is a `histogram`, and a count by two categories is a
   `heatmap`. Put alternatives in `tabs`, charts that read
   together in `columns`, and what most readers skip in a folded `section`. Prose is
   `markdown`, a process or structure is a `mermaid` diagram, and what explains one row is
   that row's `detail`. Link a row to the task and file it came from. Use `canvas` only when nothing else
   can draw it. When the reader should pick the slice (a lane, a threshold), declare
   `controls` and read `params` instead of building a tab per slice. When there is nothing to
   show yet, or only part of it, say so in a `note` rather than returning an error.
4. **Build.** See below.
5. **Look at it.** Run the pack, open the Report pane, and read every tab. See "Checking".
6. **Stop** when the view draws from a real run's files and says what was asked.

## TypeScript or JavaScript

The plugin is one module with no imports. It exports `inputs`, the captured file paths it
reads, and `view`, which returns the view as plain objects in the shapes `components.md` shows.

```ts
export const inputs = ["verdicts.json"];

export function view({ files }: ViewRequest): View {
  const rows = files.flatMap((file) => JSON.parse(file.text) as { verdict: string }[]);
  const canvas = new Canvas(240, 40);
  const ctx = canvas.getContext("2d");
  ctx.fillStyle = "bad";
  ctx.fillRect(0, 10, 240 * (rows.filter((r) => r.verdict === "affected").length / rows.length), 20);
  return {
    title: "Report",
    body: { stack: { children: [
      { stats: { items: [{ label: "Rows", value: String(rows.length) }] } },
      canvas.node(),
    ] } },
  };
}
```

- The types are in `crucible-view/js/crucible-view.d.ts`: `ViewRequest`, `View`, `ViewNode`,
  `ViewControl`, `Tone`, and `Canvas`. Check against them with
  `tsc --strict --noEmitOnError --target es2022 --module es2022 --lib es2022 crucible-view.d.ts view.ts`
  and ship the `.js` it writes. The app runs JavaScript, not TypeScript.
- `Canvas` is the browser's canvas as far as the app draws it: `new Canvas(width, height)`,
  `getContext("2d")`, then `fillStyle`, `fillRect`, `beginPath`, `arc`, `fillText` and the rest
  under their usual names. `canvas.node()` is the component. `components.md` says what is
  missing. Use `Canvas` inside `view`, not at the top of the module.
- `Cell.duration`, `Cell.bytes`, `Cell.percent`, `Cell.count` and `Cell.date` turn a number into
  a table cell that shows it formatted and sorts by the number, as `Cell::duration` and the rest
  do in Rust. Like `Canvas`, use them inside `view`, and do not name anything of yours `Cell`
  or `Canvas` at the top of the module.
- There is no `fetch`, `require`, `process`, DOM or timer. `console.log` is kept only to show
  beside an error.
- A thrown error is shown in the pane with its stack, and the line numbers are your file's.
- The engine is QuickJS, so ES2023 syntax is safe.

The app compiles the script itself each time it draws, so there is nothing to build into the
pack beyond the `.js`.

## Rust

```toml
[package]
name = "report-view"
version = "0.0.0"
edition = "2024"

[workspace]

[lib]
crate-type = ["cdylib"]

[dependencies]
crucible-view = { path = "../../../../crucible-view" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[profile.release]
opt-level = "s"
lto = true
strip = true
panic = "abort"
```

Outside the crucible repo, depend on `crucible-view` by `git` instead of `path`.

```rust
use crucible_view::{Node, Request, Stat, Tone, View};

crucible_view::plugin!(inputs = ["verdicts.json"], view = report);

fn report(request: Request) -> Result<View, String> {
    let mut rows = 0;
    for file in &request.files {
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&file.text)
            .map_err(|e| format!("{} from {}: {e}", file.path, file.task))?;
        rows += parsed.len();
    }
    Ok(View {
        title: "Report".into(),
        body: Node::Stats {
            items: vec![Stat {
                label: "Rows".into(),
                value: rows.to_string(),
                tone: Tone::Neutral,
            }],
        },
        controls: vec![],
        live: Default::default(),
    })
}
```

`file.task` is the task that captured the file, `task[instance]` for a mapped task. Return
`Err` with a sentence a person can act on; it is shown in the pane.

```
cargo build --release --target wasm32-unknown-unknown --target-dir <outside the pack>
cp <target dir>/wasm32-unknown-unknown/release/report_view.wasm ui/report.wasm
```

Keep the target directory out of the pack: the pack is saved whole. Build for
`wasm32-unknown-unknown`, not a WASI target: a module that imports anything is refused.

## Runs view

A plugin may also draw the launches of its playbook: how a result moved from one launch to the
next, which launches regressed, a trend over a week of scheduled runs. It is a second entry
beside `view`. A plugin without it has no runs view and nothing else changes.

```ts
export const runsInputs = ["verdicts.json"];

export function runs({ runs, total, params }: RunsRequest): View {
  return {
    title: `${runs.length} of ${total} launches`,
    body: { table: { id: "launches", columns: [{ id: "launch", title: "Launch" }],
      rows: runs.map((run) => ({ id: run.id, cells: [{ text: run.id, href: `launch:${run.id}` }] })) } },
  };
}
```

```rust
crucible_view::plugin!(
    inputs = ["verdicts.json"],
    view = report,
    runs_inputs = ["verdicts.json"],
    runs_view = history,
);

fn history(request: crucible_view::RunsRequest) -> Result<crucible_view::View, String> { .. }
```

The request holds the launches newest first, each with what its newest run captured of
`runsInputs`, and `total`, how many launches the reader's filters keep. A launch with no run yet
has no files. `params` works as it does for `view`. A request may gain fields; ignore the ones
you do not read.

```json runs-request
{"runs": [
  {"id": "triage:12", "status": "done", "started": 1793491200, "cost_usd": 0.42,
   "origin": "schedule", "params": {"lane": "go"},
   "files": [{"path": "verdicts.json", "task": "triage-go", "text": "[]"}]},
  {"id": "triage:11", "status": "running", "started": 1793404800, "origin": "manual",
   "pack_version": 7, "files": []}
 ],
 "total": 30}
```

`status` is `running`, `done`, `error`, `cancelled` or `unfinished`. `cost_usd` is left out while
a launch runs. `pack_version` is the draft save a draft's launch ran.

- **Where it shows.** On a playbook's runs page in the desktop app, when no launch is picked
  and the filters keep more than one, as a Report tab beside Runs. The tabs show only when the
  plugin draws a runs view. The plugin is the one in the newest launch's pack.
- **Links.** `launch:<id>` opens that launch and `launch:<id>/task/<task>` opens it on a
  task; `crucible_view::launch_link` and `launch_task_link` build them. A runs view has no one
  run, so a `run:task` or `run:file` address in a cell is refused there. A view of one run may
  hold launch links too.
- **Limits.** The newest 50 launches the filters keep. Of each, only text files of the paths
  named, each at most 8 MiB. The launches together hold at most 32 MiB; past that the oldest are
  left out, so `runs` may be shorter than `total`. A launch that has stopped and not changed is
  not read again.
- **Checking.** A local pack has one run, so the app shows no runs page for it. Build a
  `RunsRequest` by hand from real captured files in a test, as `examples/report` does, and check
  the view with `crucible-view-check --runs`. Against a controller, `CRUCIBLE_VIEW_PLUGIN=<file>`
  draws a live run and its runs page with a local plugin file in place of the pack's.

## Live views

A view can change without a new run: a stepper through a long result, a page of a big table at a
time, a game. Return `live` with the view, and the app runs the plugin again on a tick, on a key
the view asked for, or on a click on a canvas rectangle marked with `ctx.hit`. The plugin keeps
nothing between runs and has no clock, so it returns all it needs in `live.state` and reads it
back from the request's `state`, with what happened in `event`. `components.md` has the fields,
the limits and the order events are answered in; `examples/stack` is a game built this way.

```ts
export const inputs = [];

export function view({ state, event }: ViewRequest): View {
  let count = Number(state ?? "0");
  if (event !== null && event !== "tick" && "hit" in event && event.hit === "add") count += 1;
  if (event !== null && event !== "tick" && "key" in event && event.key === "up") count += 1;
  const canvas = new Canvas(200, 40);
  const ctx = canvas.getContext("2d");
  ctx.fillStyle = "muted";
  ctx.fillRect(0, 0, 60, 40);
  ctx.fillStyle = "neutral";
  ctx.fillText("+1", 20, 25);
  ctx.hit(0, 0, 60, 40, "add");
  ctx.fillText(String(count), 80, 25);
  return {
    title: "Counter",
    body: canvas.node(),
    live: { state: String(count), keys: ["up"] },
  };
}
```

In Rust the same fields are `request.state`, `request.event` (`Event::Hit`, `Event::Key`,
`Event::Tick`), `View::live` (`crucible_view::Live`) and `Context2d::hit`. The runs view is live
the same way. Check it by playing it in the app: a test can build the request with a `state` and
an `event` and call the view function as many times as a reader would.

## What a compiled plugin exports

`crucible-view`'s `plugin!` writes these for a Rust plugin. A module built another way exports
`memory` and three functions, and imports nothing:

| Export | Takes | Returns |
| --- | --- | --- |
| `crucible_alloc` | a length (`i32`) | the address of that many writable bytes (`i32`) |
| `crucible_inputs` | nothing | a JSON list of captured file paths |
| `crucible_view` | the address and length of the request JSON | the reply JSON |
| `crucible_runs_inputs` | nothing | a JSON list of the paths read from each launch; optional |
| `crucible_runs_view` | the address and length of a runs request's JSON | the reply JSON; optional |

A module draws a runs view only when it exports both optional functions.

A JSON result is returned as one `i64`: the address in the high 32 bits, the length in the low
32. The request and reply; `params` is left out until the reader changes a control, and
`state` and `event` until a live view has them:

```json request
{"files": [{"path": "verdicts.json", "task": "triage-go", "text": "[]"}],
 "params": {"lane": {"select": "go"}}}
```

A live view's request after a click on its `add` rectangle, and its reply:

```json live-request
{"files": [], "state": "3", "event": {"hit": "add"}}
```

```json reply
{"view": {"title": "Counter", "body": {"text": {"text": "4"}},
 "live": {"state": "4", "keys": ["up"]}}}
```

```json reply
{"view": {"title": "Report", "body": {"text": {"text": "Nothing triaged yet."}}}}
```

```json reply
{"error": "verdicts.json from triage-go: expected a list"}
```

## Where the plugin goes

- A script: `ui/<name>.js`, on a local pack or a controller alike.
- A compiled plugin on a local pack: `ui/<name>.wasm`.
- A compiled plugin on a controller: `ui/<name>.wasm.b64`, the same bytes as base64 text,
  because a controller's packs hold text only. `base64 -i ui/report.wasm -o ui/report.wasm.b64`
  on macOS, `base64 -w0 ui/report.wasm > ui/report.wasm.b64` on Linux. Delete the `.wasm`
  before pushing.
- A plugin a task writes during the run: capture it as `<name>.view.js` or `<name>.view.wasm`
  with `emits_files`. A local run's Report pane draws the newest one in place of the pack's,
  marked with the task that wrote it.
- One plugin per pack is shown: the first by name.
- For a compiled plugin, keep the source in the pack (`ui/<name>/`), so a reviewer can read
  what the binary was built from. Rebuild and re-encode after every source change; a stale
  binary is the usual reason a change does not show.

## Checking

Run the pack so its tasks capture files, then open it in the desktop app and show the Report
pane from `Panes`. A local pack keeps captured files in `state/files/<task>/<path>`, and the
view redraws when the plugin or those files change.

Headless, from `crucible-desktop/`, for a pack under the repo's `examples/`:

```
CRUCIBLE_REPO=$PWD/.. CRUCIBLE_TAB=report ./target/debug/crucible-playbooks --scene local \
  --script clicks.json --snap report.png --at 30 --size 1700x900
```

`clicks.json` picks the pack, then a tab:

```
[{"t": 24.0, "type": "click", "key": "pack-<pack>"},
 {"t": 26.0, "type": "click", "key": "report-tabs-<tabs id>-<label>"}]
```

A tab's key ends in its button text in lower case with spaces as dashes, so a tab labelled `All`
with count 64 is `all-64`. Packs take seconds to load in a debug build; if the image shows no
pack selected, move the clicks and `--at` later. Read the image. Do not report a view as
working from a clean build alone.

Without the app, check the view itself. `crucible-view-check` reads a view, or a plugin's
answer, on standard input and applies every rule in `components.md`, and `--runs` checks a runs
view; it does not show how the view looks. For Rust, a native test in the plugin's crate can call
the view function on real captured files and assert
`view.check(crucible_view::ViewKind::Run, &crucible_view::Limits::DEFAULT)`, or `ViewKind::Runs`
for a runs view.

When the pane shows an error:

- `No task has captured <path> yet.`: the run has not written an input, or the path differs
  from the task's `emits_files`.
- `the plugin's reply could not be read: unknown variant`: a component or field name is wrong.
  Names are snake case, and a component is `{"<name>": {...}}`.
- `the plugin's view was refused: ...`: the sentence names the rule; see `components.md`.
- `the plugin trapped: ...; it wrote: ...`: a panic or runtime fault. What follows `it wrote`
  is the plugin's own standard error.
- `the plugin imports <name>, which the host does not offer`: built for a WASI target or with
  a library that needs the system. Build for `wasm32-unknown-unknown` and remove the dependency
  that pulls it in.
- `the script does not compile: ...`: a syntax error in the `.js`, with its line.
- `reading the JavaScript engine ...`: the app has no engine module; `just desktop-js-engine`
  fetches it.

## Handing back

Say which files the view reads and which tasks write them, what each tab shows, how you built
the plugin, and what you looked at: the run, the tabs, the snapshot. Say what you did not check,
such as a live controller run or a run with no data.

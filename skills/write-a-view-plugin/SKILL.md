---
name: write-a-view-plugin
description: Give a crucible playbook pack its own view of a run (a report, dashboard, table, pivot, chart or drawing in the desktop app's Report pane) by writing a view plugin in the pack's ui/ folder, in TypeScript or JavaScript, or in Rust compiled to WebAssembly. Use when asked to add or change a pack's report, custom view, chart, table or dashboard, or to turn a run's captured files into something a person reads.
---

# Writing a view plugin

A view plugin is a file in a pack's `ui/` folder: JavaScript source, or a WebAssembly module.
The desktop app hands it the files a run's tasks captured and draws what it returns in the
Report pane. The plugin computes and the app draws: you return a tree of components from a
fixed set, never pixels or HTML.

`examples/report` is a working pack to copy from: `ui/report/` is the plugin in Rust,
`ui/report-go/` the same scatter plot in Go. `components.md` beside this file lists every
component with its JSON.

Write it in TypeScript unless the pack already has a Rust plugin. A script is text a reviewer
can read, needs no build beyond a type check, and checks in under a second. Rust gives a
smaller, faster plugin and is worth it for a view that shapes a lot of data.

## What a plugin can and cannot do

- It sees only the captured files it names, as text. A file over 8 MiB or not UTF-8 is refused.
- It has no network, filesystem, arguments or environment. Its clock counts its own readings
  and its random bytes never change, so the view must depend on the files alone.
- It runs under limits: about two seconds of work, 256 MiB of memory, a 64 MiB reply, 16 input
  paths. Past one, the pane shows the error instead of the view.
- A view the app cannot check is refused whole: an unknown component or field, a row whose
  cells do not match its columns, a number that is not finite, more than one table on screen.

## The loop

1. **Find the data.** The plugin reads files tasks declare in `emits_files`, by the path the
   task declared (`verdicts.json`, `evidence/probe.json`). If the facts you need are only in
   task output or logs, add a task that writes them to a file first. Prefer one JSON file with a
   schema (`schema_file(...)`) over scraping text.
2. **Shape it in the plugin.** Every task that captured a named path gives you one file, so a
   fan-out or a set of lanes arrives as several files to merge. Decide the row (one per ticket,
   per finding, per benchmark), dedupe, sort, count. Keep that logic here, not in a script
   someone runs after the run.
3. **Choose components.** Read `components.md`. Headline numbers are `stats`, a pivot is a
   `grid`, the rows are a `table`, a category split is `bars`, an ordered x axis is a `line`
   plot, two measures are a `scatter` plot. Put alternatives in `tabs`. Use `canvas` only when
   nothing else can draw it.
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
  `Tone`, and `Canvas`. Check against them with
  `tsc --strict --noEmitOnError --target es2022 --module es2022 --lib es2022 crucible-view.d.ts view.ts`
  and ship the `.js` it writes. The app runs JavaScript, not TypeScript.
- `Canvas` is the browser's canvas as far as the app draws it: `new Canvas(width, height)`,
  `getContext("2d")`, then `fillStyle`, `fillRect`, `beginPath`, `arc`, `fillText` and the rest
  under their usual names. `canvas.node()` is the component. `components.md` says what is
  missing. Use `Canvas` inside `view`, not at the top of the module.
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
    })
}
```

`file.task` is the task that captured the file, `task[instance]` for a mapped task. Return
`Err` with a sentence a person can act on; it is shown in the pane.

```
cargo build --release --target wasm32-unknown-unknown --target-dir <outside the pack>
cp <target dir>/wasm32-unknown-unknown/release/report_view.wasm ui/report.wasm
```

Keep the target directory out of the pack: the pack is saved whole. Do not use
`wasm32-wasip1` features the host does not answer (files, sockets, the real clock): the module
is refused when it imports them.

## Other languages

Any language that builds WebAssembly works; `crucible-view` is a convenience, not a requirement.
The module exports `memory` and three functions:

| Export | Takes | Returns |
| --- | --- | --- |
| `crucible_alloc` | a length (`i32`) | the address of that many writable bytes (`i32`) |
| `crucible_inputs` | nothing | a JSON list of captured file paths |
| `crucible_view` | the address and length of the request JSON | the reply JSON |

A JSON result is returned as one `i64`: the address in the high 32 bits, the length in the low
32. The request and reply:

```json request
{"files": [{"path": "verdicts.json", "task": "triage-go", "text": "[]"}]}
```

```json reply
{"view": {"title": "Report", "body": {"text": {"text": "Nothing triaged yet."}}}}
```

```json reply
{"error": "verdicts.json from triage-go: expected a list"}
```

Go builds with the standard toolchain: `//go:wasmexport` on the three functions, an empty
`main`, and `GOOS=wasip1 GOARCH=wasm go build -buildmode=c-shared`. Keep every buffer you hand
back reachable, or the collector frees it. A Go module is about 3 MB and takes seconds to
start, against about 200 KB for Rust, so prefer Rust for a pack that ships.

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
answer, on standard input and applies every rule in `components.md`; it does not show how the
view looks. For Rust, a native test in the plugin's crate can call the view function on real
captured files and assert `view.check(&crucible_view::Limits::DEFAULT)`.

When the pane shows an error:

- `No task has captured <path> yet.`: the run has not written an input, or the path differs
  from the task's `emits_files`.
- `the plugin's reply could not be read: unknown variant`: a component or field name is wrong.
  Names are snake case, and a component is `{"<name>": {...}}`.
- `the plugin's view was refused: ...`: the sentence names the rule; see `components.md`.
- `the plugin trapped: ...; it wrote: ...`: a panic or runtime fault. What follows `it wrote`
  is the plugin's own standard error.
- `the plugin imports <name>, which the host does not offer`: built for a target or with a
  library that needs the system. Remove the dependency that pulls it in.
- `the script does not compile: ...`: a syntax error in the `.js`, with its line.
- `reading the JavaScript engine ...`: the app has no engine module; `just desktop-js-engine`
  fetches it.

## Handing back

Say which files the view reads and which tasks write them, what each tab shows, how you built
the plugin, and what you looked at: the run, the tabs, the snapshot. Say what you did not check,
such as a live controller run or a run with no data.

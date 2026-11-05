# Captured files and reports

A task that declares `emits_files` leaves those files with its run. The run view lists them
under **Files**, shows most of them inline, and links each one. This page covers what the view
shows, how an HTML report runs, and how to let a report load a script from a CDN.

## Listing and reading files

Each file has a key: `<task>/<declared path>`, or `<task>[<instance>]/<declared path>` for an
instance of a mapped task. `crux` reads files by key:

```sh
crux run-files <run_id>
crux run-file <run_id> report/index.html --out index.html
```

`GET /api/runs/{run_id}/files` returns the list, and `GET /api/runs/{run_id}/files/{key}`
returns the bytes as `application/octet-stream`.

A link to `/runs/<run_id>/files/<key>` opens that file, and **copy link** copies it. A page
opened on such a link scrolls the panel into view. Without a key, the view opens the report from
the task latest in plan order that wrote one, where a report is `REPORT.md`, `REPORT.html`, or any
HTML file, and HTML wins within a task. Then it opens the first Markdown file, then the first
file. A file from a task whose latest attempt failed is marked **failure evidence**. Empty files
are left out of the list, and the panel counts them.

The view picks a renderer from the extension, ignoring case:

| Extension | Shown as |
| --- | --- |
| `.md` | Markdown |
| `.json` | formatted JSON |
| `.txt`, `.log` | text |
| `.png`, `.jpg`, `.jpeg`, `.gif`, `.webp`, `.avif`, `.bmp`, `.svg` | image |
| `.html`, `.htm` | an interactive document, see [HTML reports](#html-reports) |

Every file has a download link. Any other file has only that.

While a playbook run is running, the view fetches the run, its task graph, and its file list
every five seconds. When a run stops, the view fetches the task graph and the file list once
more, so the last results and files appear without a reload.

## HTML reports

An HTML file is how a playbook ships a report with charts. The view shows it in a sandboxed
frame, and its scripts run. An agent may have written the file, so the frame is isolated:

- It has its own origin. It cannot read the controller's cookies or storage, call its API, or
  navigate the page around it. It cannot open windows, submit forms, or start downloads.
- A content security policy goes in front of the document. It allows inline scripts, inline
  styles, images and fonts from `data:` URIs, and the scripts in the
  [allowlist](#allowing-a-script-from-a-cdn) by exact URL. It blocks every other request:
  `fetch`, XHR, WebSocket, and scripts, stylesheets, images, fonts, and frames from URLs. It
  also blocks `eval` and WebAssembly.
- Relative URLs resolve against the controller page, not against the run's other files. A
  report cannot load the file next to it.
- A frame that the report creates gets an origin of its own. The report and that frame cannot
  read each other's DOM or globals: `parent.Plotly` throws a `SecurityError`, and
  `frame.contentDocument` is `null`. Send data between them with `postMessage`.

To write a report that renders here, put everything in one file: scripts and styles inline,
images as `data:` URIs or inline SVG, fonts as `data:` URIs. Inline the chart library, or load
it from the allowlist with the URL exactly as listed.

Decision evidence is stricter. [RFC-0002](./rfc/RFC-0002.md#rfc-0002c-decision-evidence)
forbids every network request from it, so the allowlist does not apply there.

## Allowing a script from a CDN

The allowlist is `RUN_FILE_SCRIPTS` in `crucible-controller/ui/src/pages/runFileView.ts`:

```ts
export const RUN_FILE_SCRIPTS: readonly string[] = ['https://cdn.plot.ly/plotly-2.35.2.min.js'];
```

To allow another script, add its exact URL and run the UI tests:

```sh
cd crucible-controller/ui
bun run test
bunx playwright test e2e/run.spec.ts
```

The `RUN_FILE_SCRIPTS` test rejects an entry that is not one versioned `https` file ending in
`.js`, that has a query or a fragment, or that holds a wildcard, a space, `;`, `,`, or `'`. A
host or a directory would allow every file under it. On a public package CDN, that is any
package, so any code.

Each entry has a cost:

- Every viewer's browser requests the URL when a report loads it. The CDN sees who viewed a
  report and when.
- The policy matches a URL without its query string. A report can add data to an allowed URL
  (`?x=...`) and send it to that CDN's logs. Allow only CDNs you trust with that.
- The CDN can change the file. A report can pin the content with `integrity` and
  `crossorigin="anonymous"`, if the CDN sends CORS headers.

A report that inlines its scripts needs none of this, and it also works offline.

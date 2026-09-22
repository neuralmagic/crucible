# crucible-controller ui

The control-plane SPA (Vite + React + PatternFly), embedded into the `crucible` binary by
rust-embed at build time.

- `bun run dev` — dev server with `/api` proxied to a local controller
  (`cargo run -p crucible-controller --example demo_server`).
- `bun run check` — tsc + eslint.
- `bun run test` — vitest (use `bun run test`, not `bun test`, which is bun's own runner).
- `bun run build` — production bundle into `dist/`.

## The typed API client

`openapi.json` and `src/api/schema.d.ts` are generated, not committed. `bun run generate`
(or `just ui-types` from the repo root) emits the spec from the Rust API definitions
(`cargo run -p crucible-controller --bin openapi-spec`) and runs openapi-typescript on it.
`dev`/`check`/`test`/`build` all regenerate first via bun pre-hooks, so from a fresh clone
`bun install && bun run build` is the whole workflow — nothing to keep in sync by hand.

## Conventions

- **Errors**: use `Alert` for inline/recoverable errors inside an otherwise-working page (e.g.
  a failed mutation, a stale sub-query); use `EmptyState` + `ExclamationCircleIcon` for
  whole-page load failures (the page has nothing else to show).
- **Charts**: dashboard aggregates go through the `EChart` wrapper (`src/charts/`, deep-imports
  echarts modules to keep the bundle lean); run trajectories and sparklines are dependency-free
  hand-built inline SVG (see `RunDetailPage`'s score curve) — no charting library for those.
- **Styling**: layout and spacing live in a colocated `X.module.css` + hand-authored
  `X.module.css.d.ts` (see `IssueDetailPage`, `RunDetailPage`). Inline `style={{...}}` is reserved
  for genuinely dynamic values (computed widths, percentages, per-datum colors) — static spacing
  and color-token rules belong in the module CSS.
- **Filter controls**: use the PF6 `Select` + `MenuToggle` + `SelectList`/`SelectOption` pattern
  (see `IssuesPage`, `RunsPage`), not the legacy `FormSelect`/`FormSelectOption`.
- **Pure logic**: DOM-free helpers (formatting, parsing, derivations) live in plain `.ts` files
  colocated with their page, with a `*.test.ts` covered by vitest (see `runReport.ts` /
  `runReport.test.ts`, `adminConfig.ts` / `adminConfig.test.ts`).

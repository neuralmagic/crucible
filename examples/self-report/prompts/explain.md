EXPLAIN THIS RUN

Two triage lanes ran before you. Each captured a `verdicts.json`: a list of security tickets
with a verdict for each. Their copies are in `inputs/triage-go/` and `inputs/triage-python/`.

Write a view plugin in TypeScript that explains this run to the person who will open it: what
was triaged, what needs their attention first, and anything in the data they would want to see
drawn. You decide what the view shows and how.

At least one part of the view must be a drawing you make yourself with a `Canvas`: something
the built-in charts cannot draw, that shows this data better than a table would.

- `kit/skill/components.md` lists the components a view is made of, as the JSON your `view`
  function returns. `kit/crucible-view/js/crucible-view.d.ts` has the types and the `Canvas`.
  Read both before you write anything.
- Write the plugin as one file, `view.ts`, with no imports. It exports `inputs`, here
  `["verdicts.json"]`, and `view(request: ViewRequest): View`.
- You cannot open the app. Check your work with
  `view-check view.ts inputs/triage-go/verdicts.json inputs/triage-python/verdicts.json`.
  It type-checks the file, writes `report.view.js`, runs it on those files, and checks the view
  against every rule the app applies. Fix what it reports and run it again.

Finish when `view-check` prints `ok`. Say what the view shows.

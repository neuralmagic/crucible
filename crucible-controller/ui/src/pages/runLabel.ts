/** The label a person reads a run by: its name, or its id for a run from before names. */
export function runLabel(run: { run_id: string; name?: string | null }): string {
  return run.name ?? run.run_id;
}

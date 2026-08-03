**This pack is broker-measured.** Its gate scores on GPU hardware reached through the broker's
code-gen MCP tools (`codegen_build` → `codegen_benchmark`/`codegen_profile`), so keep the shape it
was drafted with: `[agent.broker]` enabled with a `bin`, a `[measure]` table carrying the goal's
codegen tool contract verbatim, a `measure_cmd` that drives those tools and fails closed on
missing or truncated hardware evidence, `skip_baseline = true`, and a `[judge.selftest]` that is
broker-free. Do not collapse the gate into a locally-runnable harness because that would be easier
to validate, and do not reject the issue because GPUs are unreachable from this turn.

Validation did NOT execute `measure_cmd` — it has no broker and no GPUs either. It checked the
manifest shape and ran the broker-free self-test control, so that is what the evidence below is
about, and that is what you can run yourself: ignore the later instruction to execute the
`measure_cmd` and both controls.

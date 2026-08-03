**Confirmed tier: {{TIER}}.** Fix the pack as drafted for that tier — a T1 pack's harness stays a
T1 harness (an authored, locally-runnable metric under `tools/`); do not quietly collapse it back
into a T0-shaped `go test` invocation because that would be easier to validate. If you genuinely
believe the tier is wrong for this issue, that's a `REJECTED.md` call (below), not a silent
downgrade.

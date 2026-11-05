# vllm-agent-fanout

A `seed` command builds vLLM at `vllm_ref` once through the run's own `buildit`. Agents then fan
out over CUDA kernels, one per kernel in `kernels`, at most eight at a time. Each builds the same
ref as checked out, edits its kernel, and builds again. The `trace` task renders every build into
`traces/index.html`, which the run page shows under its captured files.

## Who writes the shared cache

With `[buildit.cache] write = true`, the run's own builds may write the shared ccache. These are
the builds that `command` and `evaluate` tasks start on the run's socket. Agents' builds only read
it, unless `agent_writes = true`:

```toml
[buildit.cache]
cluster      = "spoke-a"
token_secret = "buildit-cache-token"
write        = true
```

So the seed fills the cache from a known ref and a known Dockerfile. Every agent's `checkout`
build is then served from it. An agent's `edit` compiles its one changed object and stores
nothing, so one agent's edit never reaches another agent's build.

## The pack

- `seed.py` reads its parameters from `$CRUCIBLE_INPUTS_FILE`. It checks vLLM out into
  `build/vllm/` and stops if a name in `kernels` has no `csrc/<name>_kernels.cu`. It then runs
  `buildit build` and `buildit run` on the socket with the same arguments the agents pass, and
  copies the build's ninja log, ccache stats log and step times to `traces/seed.*`. Its last
  stdout line emits `kernels`, the list `edit` maps over.
- `edit` is `isolated`, so each instance checks vLLM out in a worktree of its own. Its
  `required = False` lets the sweep finish when one kernel fails. `trace` joins with
  `join = "passed"`.
- `trace/render.py` renders the seed from `inputs/seed/` and each instance from
  `inputs/edit[<kernel>]/`. It writes one table of every build's time and ccache results, with
  each build's trace page opening in place, since a join task cannot declare a file per instance.
- `build/Dockerfile`, `trace/buildtrace.py`, `trace/trace.html` and `gitignore` are copies of the
  ones in `examples/vllm-agent-build`. A test keeps them identical.

See `examples/vllm-agent-build` for the Dockerfile, the buildit caches and the deployment settings.

# Drafting with a local agent

The desktop app's Agent pane runs a coding agent in a sandbox on an OpenShell gateway on your
machine. This is what that needs, in order. All of it is macOS with podman; the commands are the
same on Linux.

| Piece | What it is for |
| --- | --- |
| the desktop app, newer than 0.6.4 | 0.6.4 and older have no Agent pane |
| podman | runs the sandbox containers |
| OpenShell gateway and CLI, 0.1.5 or newer | creates sandboxes; the app talks to it over gRPC |
| a provider on the gateway | holds the model credential: a Claude subscription or a model gateway's key |
| `localhost/crucible-agent` | the sandbox image: Claude Code, `crux`, `crucible-lsp`, the docs |
| a signed-in profile in the app | the agent works as that user |

## 1. podman

```bash
podman machine list
```

The machine must be running. The image build compiles `crux` and `crucible-lsp`, so give the
machine at least 8 GiB of memory.

## 2. The gateway

Install the `openshell` CLI and `openshell-gateway` from the
[NVIDIA/OpenShell releases](https://github.com/NVIDIA/OpenShell/releases), 0.1.5 or newer, both
at the same version, and put them on `PATH`.

`~/.config/openshell/gateway.toml`:

```toml
[openshell]
version = 2

[openshell.gateway]
bind_address = "0.0.0.0:17670"
compute_driver = "podman"
```

Check the file, then start the gateway. It is not a service: it stops at logout or reboot.

```bash
openshell-gateway --config ~/.config/openshell/gateway.toml config preflight
```

```bash
nohup openshell-gateway --config ~/.config/openshell/gateway.toml >> ~/.local/state/openshell/gateway.log 2>&1 &
```

The app dials the gateway the CLI has active: the name in `~/.config/openshell/active_gateway`,
its endpoint in `gateways/<name>/metadata.json`, and its client certificate in
`gateways/<name>/mtls`. If the CLI can reach the gateway, so can the app:

```bash
openshell provider list
```

## 3. A provider for the harness

A provider holds the model credential on the gateway. The sandbox gets a placeholder in its
environment, and the gateway swaps in the real value on requests to that provider's endpoints
only. The gateway has no built-in profiles, so import one from this repo first. Pick one.

### Claude subscription

Claude Code on your own Claude subscription. This is what the app uses with no `[agent]`
config.

```bash
openshell provider profile import -f crucible-desktop/sandbox/claude-sub.yaml
```

```bash
claude setup-token
```

```bash
CLAUDE_CODE_OAUTH_TOKEN=<the token> openshell provider create --name claude-sub --type claude-sub --credential CLAUDE_CODE_OAUTH_TOKEN
```

### A model gateway

Claude Code against any gateway that serves the Anthropic Messages API, on that gateway's API
key. Copy `crucible-desktop/sandbox/model-gateway.yaml`, set `endpoints[0].host` to the
gateway's host, and import the copy:

```bash
openshell provider profile import -f model-gateway.yaml
```

```bash
ANTHROPIC_API_KEY=<the key> openshell provider create --name model-gateway --type model-gateway --credential ANTHROPIC_API_KEY
```

Then point the app at it, in `~/.config/crucible-playbooks/config.toml`, and restart the app:

```toml
[agent]
providers = ["model-gateway"]

[agent.env]
ANTHROPIC_BASE_URL = "https://models.example.com"
```

The model is `claude-opus-5-5` unless `[agent].model` names another one the gateway serves.
The profile sends the key as `x-api-key`, to that host only.

In both, `--credential NAME` with no value reads the variable `NAME` from the environment, and
the credential is stored under that name, which is the variable the harness reads.

## 4. The image

```bash
just desktop-agent-image
```

This builds `localhost/crucible-agent` from the repo root. Build it again after a change to
`crux`, `crucible-lsp`, `docs/` or `skills/draft-a-playbook`.

## 5. The app

Sign in to a controller profile, open a playbook's run, and on the Pack pane press `Draft` (or
`Edit`) to pull a working copy. `Agent` on the working copy's toolbar, or `cmd+9`, opens the
Agent pane. The first prompt creates the sandbox, which takes a few seconds.

## 6. Another model or harness

`[agent]` in `~/.config/crucible-playbooks/config.toml`, read when the app starts:

```toml
[agent]
model = "claude-sonnet-5-5"             # default claude-opus-5-5

[agent.env]                             # set for the harness, beside the controller's variables
ANTHROPIC_BASE_URL = "https://models.example.com"
```

A harness other than Claude Code sets the rest:

```toml
[agent]
image = "localhost/my-agent"            # runs as a non-root user that owns its working directory
providers = ["openrouter"]
model = "glm-5-3"
format = "opencode"                     # how stdout reads: claude, codex, opencode, pi or text
command = ["my-agent", "--mcp", "{mcp_url}", "--mcp-token", "{mcp_token}", "-m", "{model}", "{prompt}"]
binaries = ["/usr/local/bin/my-agent", "/usr/local/bin/crux"]
```

`binaries` are the programs allowed to reach the controller relay. The harness finds the relay
in `CONTROLLER_URL` and `CONTROLLER_API_TOKEN` (what `crux` reads) and in `CRUCIBLE_MCP_URL` and
`CRUCIBLE_MCP_TOKEN`.

## When it does not work

The pane's status row carries the reason in red.

| What you see | Cause | Fix |
| --- | --- | --- |
| `The OpenShell gateway at ... did not answer` | the gateway is not running, or the CLI's active gateway is another one | start it (step 2); `openshell gateway select <name>` |
| `No OpenShell gateway is registered on this machine` | `~/.config/openshell/active_gateway` or its `gateways/<name>` is missing | register the gateway with the CLI |
| `The OpenShell gateway is 0.0.x; drafting with an agent needs 0.1.5 or newer` | an old gateway binary is the one running | install 0.1.5 or newer and restart it |
| `WorkspaceValidationFailed: OCI WorkingDir is not usable by the sandbox identity` | the image has no non-root `USER` | add a `USER` that owns the working directory |
| provider not found | the provider named in `[agent]` (default `claude-sub`) is not on the gateway | step 3 |
| the agent's first call to the controller is denied | the harness binary is not in `binaries` | add its path |
| every sandbox create fails after a gateway upgrade | provider profiles written by an older gateway do not decode | export them on the old version, delete them, import them on the new one |
| the turn ends at once with an auth error | the stored token expired | `claude setup-token`, then `openshell provider update claude-sub --credential CLAUDE_CODE_OAUTH_TOKEN` with the token in that variable |
| the agent has no memory of the last prompt | the sandbox was replaced (app restart, `Reset`, or the gateway restarted) | nothing; a session lives as long as its sandbox |

Sandboxes are named `cr-draft-*`. `End` deletes the session's sandbox, and one that outlives the
app is deleted when the next session starts.

```bash
openshell sandbox list
```

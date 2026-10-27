# Editor support

`crucible-lsp` is a language server for crucible's workflow files (`workflow.star` and any `.star`
files it loads). It runs the same compiler as `crucible`, so the errors you see in your editor are
the errors `crucible` would give you.

## Install the server

```bash
just lsp-install
```

This installs `crucible-lsp` into `~/.cargo/bin`. Every editor below starts it from there. Run it
again after you pull changes to the workflow language.

## Neovim (0.11+)

Create `~/.config/nvim/lsp/crucible_lsp.lua`:

```lua
return {
  cmd = { "crucible-lsp" },
  filetypes = { "starlark" },
  root_markers = { "crucible.toml" },
  workspace_required = true,
}
```

Then add `vim.lsp.enable("crucible_lsp")` to `init.lua`. Neovim already treats `*.star` files as
`starlark`. For syntax highlighting, install the tree-sitter parser with `:TSInstall starlark`.

## VS Code

```bash
just vscode-install
```

This builds the extension in `editors/vscode` and installs it. It adds syntax highlighting for
`*.star` files and starts `crucible-lsp`. To use a different build of the server, set
`crucible.server.path`.

## Claude Code (CLI and desktop)

```bash
just claude-install
```

This installs a Claude Code plugin that starts `crucible-lsp` for `.star` files. Claude then sees
errors and warnings after each edit, and can look up definitions, references, and symbols. New
sessions pick it up automatically; in a session that is already open, run `/reload-plugins`.

The plugin is installed from this checkout's path. If you delete the checkout, run
`just claude-install` again from the one you keep.

## Other editors

Any editor with an LSP client can run `crucible-lsp` for `*.star` files. We have not tested the
setups below.

### Emacs (29+, eglot)

```elisp
(define-derived-mode crucible-starlark-mode python-mode "Starlark")
(add-to-list 'auto-mode-alist '("\\.star\\'" . crucible-starlark-mode))
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs '(crucible-starlark-mode . ("crucible-lsp"))))
(add-hook 'crucible-starlark-mode-hook #'eglot-ensure)
```

The new mode borrows Python's highlighting, and keeps the server away from your Python files. If
you use lsp-mode instead of eglot:

```elisp
(with-eval-after-load 'lsp-mode
  (add-to-list 'lsp-language-id-configuration '(crucible-starlark-mode . "starlark"))
  (lsp-register-client
   (make-lsp-client :new-connection (lsp-stdio-connection "crucible-lsp")
                    :activation-fn (lsp-activate-on "starlark")
                    :server-id 'crucible-lsp)))
(add-hook 'crucible-starlark-mode-hook #'lsp-deferred)
```

### Helix

Add to `~/.config/helix/languages.toml` (Helix already highlights `*.star` files):

```toml
[language-server.crucible-lsp]
command = "crucible-lsp"

[[language]]
name = "starlark"
roots = ["crucible.toml"]
language-servers = ["crucible-lsp"]
```

### Vim (vim-lsp)

Vim already treats `*.star` files as `starlark`:

```vim
if executable('crucible-lsp')
  autocmd User lsp_setup call lsp#register_server({
        \ 'name': 'crucible-lsp',
        \ 'cmd': {server_info -> ['crucible-lsp']},
        \ 'allowlist': ['starlark'],
        \ })
endif
```

## Features

- **Errors as you type.** Every edit recompiles the workflow. An error inside a loaded `.star`
  file shows up in that file. Required parameters get stand-in values so the rest still checks.
- **Completion** for:
  - the functions your workflow `type` allows, and the arguments you haven't written yet
  - argument values such as `join = "passed"`
  - file paths in `prompt_file`, `schema_file`, `load`, `template`, and `skill`
  - parameter names in `param("...")`, and sandbox names from `crucible.toml`
  - a task's output fields after `task.`, and a route's questions after `route.`
  - variables, functions, and builtins
- **Hover** docs for functions, arguments, parameters, and variables.
- **Signature help** while you fill in a call.
- **Go to definition** for variables, loaded names, files, parameters, task names, and output
  fields.
- **Find references** and a document outline.
- **Only crucible files.** A `.star` file with no `crucible.toml` above it and no `workflow()`
  call gets no diagnostics, so Bazel and Tilt files are left alone.

## Lints

| Code | Severity | What it catches |
| --- | --- | --- |
| `unknown-argument` | error | A misspelled argument, with a suggested fix. Shows every one, not just the first. |
| `wrong-lane` | error | A function your workflow `type` doesn't support. |
| `missing-pack-file` | error | A file path that doesn't exist, or that leaves the pack with `..`. |
| `undeclared-sandbox` | error | A `sandbox =` that `crucible.toml` doesn't define. |
| `shadowed-builtin` | warning | A variable that hides a built-in function. |
| `unused-param` | warning | A parameter nothing reads: no `param()` call and no command task. |
| `redundant-default` | hint | An argument set to the value it would have anyway. |

Starlark's own linter adds `unused-load`, `unused-assign`, `using-undefined`, and a few others.
To silence a lint on one line, add `# starlark-lint-disable <code>`.

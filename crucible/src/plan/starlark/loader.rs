//! `load()` resolution, confined to the pack directory.
//!
//! A `FrozenModule` has to exist before the evaluator that loads it starts, so the whole `load()`
//! tree is resolved, evaluated, and frozen up front, depth-first in source order. Every module in
//! the tree shares one [`dsl::CompileState`], so prompt budgets, session ordering, and the
//! constructed-task ledger span a workflow and its libraries alike.

use std::collections::HashMap;
use std::path::PathBuf;

use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark_syntax::codemap::FileSpan;
use starlark_syntax::syntax::AstModule;

use crate::errors::FileError;
use crate::plan::starlark as dsl;

/// The pre-resolved module map. Keys are the raw `load()` strings, so two spellings of one file
/// hold two entries pointing at the same frozen module.
pub(crate) struct PackLoader<'a> {
    state: &'a dsl::CompileState,
    modules: HashMap<String, FrozenModule>,
}

impl FileLoader for PackLoader<'_> {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        match self.modules.get(path) {
            Some(module) => Ok(module.clone()),
            None => Err(self.state.throw(dsl::CompileError::LoadUnresolved {
                raw: path.to_owned(),
            })),
        }
    }
}

/// Resolve every `load()` reachable from `root`. `root_bytes` seeds the shared source budget.
pub(crate) fn resolve<'a>(
    root: &AstModule,
    state: &'a dsl::CompileState,
    globals: &Globals,
    root_bytes: usize,
    lane: crate::plan::workflow::WorkflowType,
) -> dsl::Result<PackLoader<'a>> {
    let mut resolver = Resolver {
        state,
        lane,
        globals,
        modules: HashMap::new(),
        by_path: HashMap::new(),
        active: Vec::new(),
        admitted: 0,
        total_bytes: root_bytes,
    };
    resolver.resolve_all(root)?;
    Ok(PackLoader {
        state,
        modules: resolver.modules,
    })
}

struct Resolver<'a, 'g> {
    state: &'a dsl::CompileState,
    lane: crate::plan::workflow::WorkflowType,
    globals: &'g Globals,
    modules: HashMap<String, FrozenModule>,
    /// Frozen modules by canonical path, so two spellings of one file are evaluated once.
    by_path: HashMap<PathBuf, FrozenModule>,
    /// Canonical paths of the modules currently being evaluated. Its length is the Rust
    /// recursion depth of `resolve_one -> evaluate -> resolve_all -> resolve_one`.
    active: Vec<PathBuf>,
    /// Modules admitted so far. `by_path` cannot serve as this count: it is written only after
    /// the recursive `evaluate` returns, so while descending a chain it is empty at every
    /// level, and a chain of any length passed a budget that refused 33 siblings.
    admitted: usize,
    total_bytes: usize,
}

impl Resolver<'_, '_> {
    fn resolve_all(&mut self, module: &AstModule) -> dsl::Result<()> {
        for load in module.loads() {
            self.resolve_one(load.module_id, &load.span)?;
        }
        Ok(())
    }

    fn resolve_one(&mut self, raw: &str, at: &FileSpan) -> dsl::Result<()> {
        if self.modules.contains_key(raw) {
            return Ok(());
        }
        let located = |error: dsl::CompileError| dsl::CompileError::At {
            at: at.clone(),
            inner: Box::new(error),
        };
        let (_, canonical) = self
            .state
            .context_mut()
            .resolve_in_pack(raw, dsl::PathKind::Module)
            .map_err(|rejection| located(rejection.at(dsl::PathKind::Module, raw)))?;
        if self.active.contains(&canonical) {
            return Err(located(dsl::CompileError::LoadCycle {
                raw: raw.to_owned(),
            }));
        }
        if let Some(existing) = self.by_path.get(&canonical) {
            let existing = existing.clone();
            self.modules.insert(raw.to_owned(), existing);
            return Ok(());
        }
        // Counted here, on the way down, so a chain is bounded exactly as a fan-out is. This
        // also bounds the recursion below it, which `active` re-checks independently so that a
        // later change to the counting cannot quietly reopen the stack.
        if self.admitted >= dsl::MAX_LOAD_MODULES {
            return Err(located(dsl::CompileError::LoadBudgetSpent));
        }
        if self.active.len() >= dsl::MAX_LOAD_MODULES {
            return Err(located(dsl::CompileError::LoadBudgetSpent));
        }
        self.admitted += 1;
        // Size is checked from the directory entry, before the bytes are read: reading first
        // and refusing after is the read the limit exists to prevent.
        let declared = std::fs::metadata(&canonical)
            .map_err(FileError::at("reading loaded module", &canonical))
            .map_err(|error| located(dsl::CompileError::File(error)))?
            .len();
        if declared > dsl::MAX_SOURCE_BYTES as u64 {
            return Err(located(dsl::CompileError::SourceTooLarge {
                bytes: declared as usize,
            }));
        }
        let source = std::fs::read_to_string(&canonical)
            .map_err(FileError::at("reading loaded module", &canonical))
            .map_err(|error| located(dsl::CompileError::File(error)))?;
        if source.len() > dsl::MAX_SOURCE_BYTES {
            return Err(located(dsl::CompileError::SourceTooLarge {
                bytes: source.len(),
            }));
        }
        self.total_bytes = self.total_bytes.saturating_add(source.len());
        if self.total_bytes > dsl::MAX_TOTAL_SOURCE_BYTES {
            return Err(located(dsl::CompileError::LoadSourceBudgetSpent));
        }
        dsl::reject_deep_nesting(&source, &canonical).map_err(&located)?;
        let ast = AstModule::parse(&canonical.display().to_string(), source, &dsl::dialect())
            .map_err(|error| located(dsl::parse_error(error)))?;
        self.active.push(canonical.clone());
        let frozen = self.evaluate(ast);
        self.active.pop();
        let frozen = frozen?;
        self.by_path.insert(canonical, frozen.clone());
        self.modules.insert(raw.to_owned(), frozen);
        Ok(())
    }

    /// Resolve the module's own loads, evaluate it, and freeze it.
    fn evaluate(&mut self, ast: AstModule) -> dsl::Result<FrozenModule> {
        self.resolve_all(&ast)?;
        let idents = dsl::idents::scan(&ast, self.lane);
        let loader = PackLoader {
            state: self.state,
            modules: self.modules.clone(),
        };
        let state = self.state;
        let globals = self.globals;
        Module::with_temp_heap(|module| -> dsl::Result<FrozenModule> {
            {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(state);
                eval.set_loader(&loader);
                dsl::budgets(&mut eval)?;
                eval.eval_module(ast, globals).map_err(|error| {
                    state
                        .take_thrown()
                        .map(|thrown| dsl::idents::narrow(thrown, &idents))
                        .unwrap_or_else(|| dsl::idents::map_error(&error, &idents))
                })?;
            }
            module
                .freeze()
                .map_err(|error| dsl::CompileError::Eval(error.err_msg))
        })
    }
}

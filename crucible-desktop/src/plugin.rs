//! Runs a pack's view plugin: a WebAssembly module with no imports, interpreted under a fuel
//! and memory limit. Only the JavaScript engine module may import the WASI calls [`wasi`]
//! answers with nothing. Every call gets a fresh instance.

mod js;
mod wasi;

pub use crate::plugin::js::Script;

use std::path::{Component, Path};

use crucible_view::{
    ALLOC, INPUTS, RUNS_INPUTS, RUNS_VIEW, Reply, Request, RunsRequest, VIEW, View, ViewKind,
};
use serde::Serialize;
use wasmi::{Config, Engine, Instance, Linker, Memory, Store, StoreLimitsBuilder, TrapCode};

use crate::plugin::wasi::Held;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The interpreter fuel one call may burn. A billion is about 20 ms of work in a release
    /// build.
    pub fuel: u64,
    pub memory_bytes: usize,
    /// Entries one table may hold.
    pub table_elements: usize,
    pub reply_bytes: usize,
    pub inputs: usize,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        fuel: 100_000_000_000,
        memory_bytes: 256 << 20,
        table_elements: 1 << 20,
        reply_bytes: 64 << 20,
        inputs: 16,
    };
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Fault {
    #[error("not a plugin the host can load: {0}")]
    Module(String),
    #[error("the plugin imports {0}, which the host does not offer")]
    Import(String),
    #[error("the plugin does not export {0}")]
    Export(&'static str),
    #[error("the plugin ran past its instruction limit")]
    Fuel,
    #[error("the plugin ran past its memory limit")]
    Memory,
    #[error("the plugin trapped: {0}")]
    Trap(String),
    #[error("the plugin's reply could not be read: {0}")]
    Reply(String),
    #[error("the plugin asks for {0}, which is not a path inside a task's captured files")]
    Input(String),
    #[error("the plugin asks for more than {0} files")]
    Inputs(usize),
    #[error("the script does not compile: {0}")]
    Script(String),
    #[error("{0}")]
    Refused(String),
    #[error("the plugin's view was refused: {0}")]
    View(#[from] crucible_view::Problem),
}

/// A function a plugin offers that builds a view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
    /// The view of a run.
    View,
    /// The view of the launches of the playbook.
    Runs,
}

impl Entry {
    /// The module's export for it.
    fn export(self) -> &'static str {
        match self {
            Entry::View => VIEW,
            Entry::Runs => RUNS_VIEW,
        }
    }

    /// The key a script is asked under.
    fn key(self) -> &'static str {
        match self {
            Entry::View => "view",
            Entry::Runs => "runs_view",
        }
    }

    /// What the view it builds is checked as.
    fn kind(self) -> ViewKind {
        match self {
            Entry::View => ViewKind::Run,
            Entry::Runs => ViewKind::Runs,
        }
    }
}

/// A view plugin of either kind: a compiled module, or JavaScript source.
pub enum Loaded {
    Wasm(Module),
    Js(Script),
}

impl Loaded {
    /// The captured file paths the plugin reads.
    pub fn inputs(&self) -> Result<Vec<String>, Fault> {
        let paths = match self {
            Loaded::Wasm(module) => module.inputs(INPUTS)?,
            Loaded::Js(script) => script.inputs()?,
        };
        self.admit(paths)
    }

    /// The captured file paths the plugin reads from each launch of its playbook; `None` when
    /// it draws no view of them.
    pub fn runs_inputs(&self) -> Result<Option<Vec<String>>, Fault> {
        let paths = match self {
            Loaded::Wasm(module) if module.exports(&[RUNS_INPUTS, RUNS_VIEW]) => {
                Some(module.inputs(RUNS_INPUTS)?)
            }
            Loaded::Wasm(_) => None,
            Loaded::Js(script) => script.runs_inputs()?,
        };
        paths.map(|paths| self.admit(paths)).transpose()
    }

    /// The plugin's view of `request`, checked against the view limits.
    pub fn view(&self, request: &Request) -> Result<View, Fault> {
        self.ask(Entry::View, request)
    }

    /// The plugin's view of the launches in `request`, checked as a runs view.
    pub fn runs_view(&self, request: &RunsRequest) -> Result<View, Fault> {
        self.ask(Entry::Runs, request)
    }

    /// `paths` when there are few enough and each is inside a task's captured files.
    fn admit(&self, paths: Vec<String>) -> Result<Vec<String>, Fault> {
        let limits = self.limits();
        if paths.len() > limits.inputs {
            return Err(Fault::Inputs(limits.inputs));
        }
        match paths.iter().find(|path| !captured(path)) {
            Some(path) => Err(Fault::Input(path.clone())),
            None => Ok(paths),
        }
    }

    /// What `entry` builds from `request`, checked against the view limits.
    fn ask(&self, entry: Entry, request: &impl Serialize) -> Result<View, Fault> {
        let reply = match self {
            Loaded::Wasm(module) => {
                let body = serde_json::to_vec(request).map_err(|e| Fault::Reply(e.to_string()))?;
                module.answer(entry.export(), &body)?
            }
            Loaded::Js(script) => script.answer(entry.key(), request)?,
        };
        match reply {
            Reply::View(view) => {
                view.check(entry.kind(), &crucible_view::Limits::DEFAULT)?;
                Ok(view)
            }
            Reply::Error(error) => Err(Fault::Refused(error)),
        }
    }

    fn limits(&self) -> &Limits {
        match self {
            Loaded::Wasm(module) => &module.limits,
            Loaded::Js(script) => script.limits(),
        }
    }
}

/// A module and the limits each of its calls runs under: a plugin, or the engine a script runs
/// in.
pub struct Module {
    engine: Engine,
    module: wasmi::Module,
    limits: Limits,
    imports: Imports,
}

/// What a module may import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Imports {
    Nothing,
    /// The WASI calls the host answers.
    Wasi,
}

/// The most tables one plugin may have.
const TABLES: usize = 8;

struct Call {
    store: Store<Held>,
    instance: Instance,
    memory: Memory,
}

impl Module {
    /// A pack's plugin, which imports nothing.
    pub fn load(wasm: &[u8], limits: Limits) -> Result<Module, Fault> {
        Module::new(wasm, limits, Imports::Nothing)
    }

    /// The JavaScript engine, which may import the WASI calls the host answers.
    pub fn engine(wasm: &[u8], limits: Limits) -> Result<Module, Fault> {
        Module::new(wasm, limits, Imports::Wasi)
    }

    fn new(wasm: &[u8], limits: Limits, imports: Imports) -> Result<Module, Fault> {
        let mut config = Config::default();
        config.consume_fuel(true).wasm_simd(true);
        let engine = Engine::new(&config);
        let module = wasmi::Module::new(&engine, wasm).map_err(|e| Fault::Module(e.to_string()))?;
        let foreign = module.imports().find(|import| match imports {
            Imports::Nothing => true,
            Imports::Wasi => import.module() != wasi::MODULE || !wasi::offers(import.name()),
        });
        match foreign {
            Some(import) => Err(Fault::Import(format!(
                "{}.{}",
                import.module(),
                import.name()
            ))),
            None => Ok(Module {
                engine,
                module,
                limits,
                imports,
            }),
        }
    }

    /// Whether the module exports every one of `names`.
    fn exports(&self, names: &[&str]) -> bool {
        names
            .iter()
            .all(|name| self.module.exports().any(|export| export.name() == *name))
    }

    /// The paths the plugin's inputs export `export` names.
    fn inputs(&self, export: &'static str) -> Result<Vec<String>, Fault> {
        let mut call = self.start()?;
        let ask = call
            .instance
            .get_typed_func::<(), u64>(&call.store, export)
            .map_err(|_| Fault::Export(export))?;
        let packed = ask
            .call(&mut call.store, ())
            .map_err(|e| said(fault(e), &call.store))?;
        serde_json::from_slice(&call.read(packed, &self.limits)?)
            .map_err(|e| Fault::Reply(e.to_string()))
    }

    /// What the plugin's `export` answers to `body`, written into its memory.
    fn answer(&self, export: &'static str, body: &[u8]) -> Result<Reply, Fault> {
        let mut call = self.start()?;
        let len = u32::try_from(body.len()).map_err(|_| Fault::Memory)?;
        let alloc = call
            .instance
            .get_typed_func::<u32, u32>(&call.store, ALLOC)
            .map_err(|_| Fault::Export(ALLOC))?;
        let build = call
            .instance
            .get_typed_func::<(u32, u32), u64>(&call.store, export)
            .map_err(|_| Fault::Export(export))?;
        let at = alloc
            .call(&mut call.store, len)
            .map_err(|e| said(fault(e), &call.store))?;
        call.memory
            .write(&mut call.store, at as usize, body)
            .map_err(|e| Fault::Trap(e.to_string()))?;
        let packed = build
            .call(&mut call.store, (at, len))
            .map_err(|e| said(fault(e), &call.store))?;
        serde_json::from_slice(&call.read(packed, &self.limits)?)
            .map_err(|e| Fault::Reply(e.to_string()))
    }

    /// A fresh instance under the module's limits, with nothing on its standard input.
    fn start(&self) -> Result<Call, Fault> {
        let limits = &self.limits;
        let store_limits = StoreLimitsBuilder::new()
            .memory_size(limits.memory_bytes)
            .memories(1)
            .table_elements(limits.table_elements)
            .tables(TABLES)
            .instances(1)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(&self.engine, Held::new(store_limits, limits.reply_bytes));
        store.limiter(|held| &mut held.limits);
        store
            .set_fuel(limits.fuel)
            .map_err(|e| Fault::Module(e.to_string()))?;
        let mut linker = Linker::new(&self.engine);
        if self.imports == Imports::Wasi {
            wasi::link(&mut linker).map_err(|e| Fault::Module(e.to_string()))?;
        }
        let instance = linker
            .instantiate_and_start(&mut store, &self.module)
            .map_err(|e| said(fault(e), &store))?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or(Fault::Export("memory"))?;
        Ok(Call {
            store,
            instance,
            memory,
        })
    }
}

impl Call {
    /// The bytes a plugin returned as an address in the high half and a length in the low half.
    fn read(&self, packed: u64, limits: &Limits) -> Result<Vec<u8>, Fault> {
        let at = (packed >> 32) as usize;
        let len = (packed & u64::from(u32::MAX)) as usize;
        if len > limits.reply_bytes {
            return Err(Fault::Reply(format!(
                "{len} bytes, over the {} allowed",
                limits.reply_bytes
            )));
        }
        let mut bytes = vec![0; len];
        self.memory
            .read(&self.store, at, &mut bytes)
            .map_err(|e| Fault::Reply(e.to_string()))?;
        Ok(bytes)
    }
}

fn fault(error: wasmi::Error) -> Fault {
    match error.as_trap_code() {
        Some(TrapCode::OutOfFuel) => Fault::Fuel,
        Some(TrapCode::GrowthOperationLimited) => Fault::Memory,
        _ => Fault::Trap(error.to_string()),
    }
}

/// A trap with what the plugin wrote before it, which is where a runtime says why it stopped.
fn said(fault: Fault, store: &Store<Held>) -> Fault {
    let wrote = String::from_utf8_lossy(&store.data().said);
    match fault {
        Fault::Trap(why) if !wrote.trim().is_empty() => {
            Fault::Trap(format!("{why}; it wrote: {}", wrote.trim()))
        }
        other => other,
    }
}

/// Whether `path` names a file below a task's captured files.
fn captured(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use crucible_view::{File, Node, Request, RunsRequest};

    use crate::plugin::{Fault, Limits, Loaded, Module};

    const SMALL: Limits = Limits {
        fuel: 5_000_000,
        memory_bytes: 2 << 20,
        table_elements: 1024,
        reply_bytes: 4096,
        inputs: 2,
    };

    /// A plugin whose `crucible_inputs` and `crucible_view` both return the string at address 8,
    /// with `body` standing in for `crucible_view`'s instructions before it returns.
    fn module(reply: &str, body: &str) -> String {
        let len = reply.len();
        let reply = reply.replace('\\', "\\\\").replace('"', "\\\"");
        format!(
            r#"(module
                (memory (export "memory") 1)
                (data (i32.const 8) "{reply}")
                (func (export "crucible_alloc") (param i32) (result i32) (i32.const 1024))
                (func (export "crucible_inputs") (result i64)
                    (i64.or (i64.shl (i64.const 8) (i64.const 32)) (i64.const {len})))
                (func (export "crucible_view") (param i32 i32) (result i64)
                    {body}
                    (i64.or (i64.shl (i64.const 8) (i64.const 32)) (i64.const {len}))))"#
        )
    }

    fn load(reply: &str, body: &str) -> Loaded {
        Loaded::Wasm(Module::load(module(reply, body).as_bytes(), SMALL).unwrap())
    }

    #[test]
    fn a_plugin_names_its_inputs_and_returns_a_checked_view() {
        let plugin = load(r#"["verdicts.json","evidence/probe.json"]"#, "");
        assert_eq!(
            plugin.inputs().unwrap(),
            ["verdicts.json", "evidence/probe.json"]
        );
        let plugin = load(
            r#"{"view":{"title":"T","body":{"text":{"text":"hi"}}}}"#,
            "",
        );
        let view = plugin.view(&Request::default()).unwrap();
        assert_eq!(view.title, "T");
        assert!(matches!(view.body, Node::Text { .. }));
    }

    /// A plugin that also exports `crucible_runs_inputs` and `crucible_runs_view`, both
    /// returning `reply`, as `crucible_view` does.
    fn with_runs(reply: &str) -> Loaded {
        let wat = module(reply, "");
        let len = reply.len();
        let runs = format!(
            r#"(func (export "crucible_runs_inputs") (result i64)
                    (i64.or (i64.shl (i64.const 8) (i64.const 32)) (i64.const {len})))
                (func (export "crucible_runs_view") (param i32 i32) (result i64)
                    (i64.or (i64.shl (i64.const 8) (i64.const 32)) (i64.const {len}))))"#
        );
        let open = wat.trim_end().strip_suffix(')').unwrap();
        let wat = format!("{open}{runs}");
        Loaded::Wasm(Module::load(wat.as_bytes(), SMALL).unwrap())
    }

    #[test]
    fn a_plugin_without_both_runs_exports_draws_no_runs_view() {
        let plugin = load(r#"["verdicts.json"]"#, "");
        assert_eq!(plugin.runs_inputs(), Ok(None));
        assert_eq!(
            plugin.runs_view(&RunsRequest::default()),
            Err(Fault::Export("crucible_runs_view"))
        );
        let half = r#"(module
            (memory (export "memory") 1)
            (data (i32.const 8) "[]")
            (func (export "crucible_runs_inputs") (result i64)
                (i64.or (i64.shl (i64.const 8) (i64.const 32)) (i64.const 2))))"#;
        let plugin = Loaded::Wasm(Module::load(half.as_bytes(), SMALL).unwrap());
        assert_eq!(plugin.runs_inputs(), Ok(None), "inputs with no view");

        let plugin = with_runs(r#"["verdicts.json","notes/a.json"]"#);
        assert_eq!(
            plugin.runs_inputs(),
            Ok(Some(vec!["verdicts.json".into(), "notes/a.json".into()]))
        );
        assert_eq!(plugin.inputs().unwrap(), ["verdicts.json", "notes/a.json"]);
    }

    #[test]
    fn runs_inputs_are_held_to_the_same_rules_as_inputs() {
        for path in ["../session.jsonl", "/etc/passwd", "a/../../b", ""] {
            let plugin = with_runs(&serde_json::to_string(&[path]).unwrap());
            assert_eq!(
                plugin.runs_inputs(),
                Err(Fault::Input(path.into())),
                "{path}"
            );
        }
        assert_eq!(
            with_runs(r#"["a","b","c"]"#).runs_inputs(),
            Err(Fault::Inputs(2))
        );
        assert_eq!(
            with_runs(r#"["a","b"]"#)
                .runs_inputs()
                .unwrap()
                .unwrap()
                .len(),
            2
        );
        assert!(matches!(
            with_runs("not json").runs_inputs(),
            Err(Fault::Reply(_))
        ));
    }

    #[test]
    fn a_runs_view_is_checked_as_one_and_a_run_s_view_as_the_other() {
        let linked = |href: &str| {
            format!(
                r#"{{"view":{{"title":"T","body":{{"table":{{"id":"t","columns":[{{"id":"a","title":"A"}}],"rows":[{{"id":"1","cells":[{{"text":"x","href":"{href}"}}]}}]}}}}}}}}"#
            )
        };
        let runs = RunsRequest::default();
        for href in [
            "launch:PB-1",
            "launch:PB-1/task/apply",
            "https://example.com",
        ] {
            let plugin = with_runs(&linked(href));
            assert!(plugin.runs_view(&runs).is_ok(), "{href}");
            assert!(plugin.view(&Request::default()).is_ok(), "{href}");
        }
        for href in ["run:task/apply", "run:file/apply/verdicts.json"] {
            let plugin = with_runs(&linked(href));
            assert_eq!(
                plugin.runs_view(&runs),
                Err(Fault::View(crucible_view::Problem::RunLink(href.into()))),
                "{href}"
            );
            assert!(plugin.view(&Request::default()).is_ok(), "{href}");
        }
        let refused = with_runs(r#"{"error":"no launches yet"}"#);
        assert_eq!(
            refused.runs_view(&runs),
            Err(Fault::Refused("no launches yet".into()))
        );
    }

    #[test]
    fn the_request_is_written_where_the_plugin_allocated() {
        let echo = r#"(module
            (memory (export "memory") 1)
            (func (export "crucible_alloc") (param i32) (result i32) (i32.const 1024))
            (func (export "crucible_view") (param i32 i32) (result i64)
                (i64.or (i64.shl (i64.extend_i32_u (local.get 0)) (i64.const 32))
                        (i64.extend_i32_u (local.get 1)))))"#;
        let plugin = Loaded::Wasm(Module::load(echo.as_bytes(), SMALL).unwrap());
        let request = Request {
            params: Default::default(),
            files: vec![File {
                path: "verdicts.json".into(),
                task: "triage".into(),
                text: "[]".into(),
            }],
        };
        let Err(Fault::Reply(why)) = plugin.view(&request) else {
            panic!("a request is not a reply");
        };
        assert!(why.contains("unknown variant `files`"), "{why}");
        assert_eq!(plugin.inputs(), Err(Fault::Export("crucible_inputs")));
    }

    fn importing(module: &str, name: &str) -> String {
        format!(
            r#"(module
                (import "{module}" "{name}" (func (param i32 i32) (result i32)))
                (memory (export "memory") 1))"#
        )
    }

    #[test]
    fn a_pack_s_plugin_may_import_nothing() {
        for (module, name) in [
            ("wasi_snapshot_preview1", "random_get"),
            ("wasi_snapshot_preview1", "fd_write"),
            ("wasi_snapshot_preview1", "clock_time_get"),
            ("wasi_snapshot_preview1", "path_open"),
            ("env", "fetch"),
        ] {
            assert_eq!(
                Module::load(importing(module, name).as_bytes(), SMALL).err(),
                Some(Fault::Import(format!("{module}.{name}"))),
            );
        }
        for (kind, name) in [
            ("(memory 1)", "memory"),
            ("(table 1 funcref)", "table"),
            ("(global i32)", "base"),
        ] {
            let wat = format!(r#"(module (import "env" "{name}" {kind}))"#);
            assert_eq!(
                Module::load(wat.as_bytes(), SMALL).err(),
                Some(Fault::Import(format!("env.{name}"))),
            );
        }
        let bare = r#"(module (memory (export "memory") 1))"#;
        assert!(Module::load(bare.as_bytes(), SMALL).is_ok());
        assert!(matches!(
            Module::load(b"not wasm", SMALL),
            Err(Fault::Module(_))
        ));
    }

    #[test]
    fn the_engine_may_import_only_the_wasi_calls_the_host_answers() {
        for (module, name) in [
            ("wasi_snapshot_preview1", "path_open"),
            ("wasi_snapshot_preview1", "sock_send"),
            ("wasi_snapshot_preview1", "fd_readdir"),
            ("wasi_snapshot_preview1", "fd_prestat_get"),
            ("env", "fetch"),
            ("env", "random_get"),
        ] {
            assert_eq!(
                Module::engine(importing(module, name).as_bytes(), SMALL).err(),
                Some(Fault::Import(format!("{module}.{name}"))),
            );
        }
        for name in [
            "clock_time_get",
            "environ_get",
            "environ_sizes_get",
            "fd_close",
            "fd_fdstat_get",
            "fd_read",
            "fd_seek",
            "fd_write",
            "proc_exit",
            "random_get",
        ] {
            let wat = format!(
                r#"(module
                    (import "wasi_snapshot_preview1" "{name}" (func))
                    (memory (export "memory") 1))"#
            );
            assert!(Module::engine(wat.as_bytes(), SMALL).is_ok(), "{name}");
        }
        assert!(matches!(
            Module::engine(b"not wasm", SMALL),
            Err(Fault::Module(_))
        ));
    }

    #[test]
    fn the_wasi_calls_give_a_plugin_nothing() {
        let probe = r#"(module
            (import "wasi_snapshot_preview1" "clock_time_get"
                (func $clock (param i32 i64 i32) (result i32)))
            (import "wasi_snapshot_preview1" "random_get" (func $random (param i32 i32) (result i32)))
            (import "wasi_snapshot_preview1" "fd_fdstat_get"
                (func $stat (param i32 i32) (result i32)))
            (import "wasi_snapshot_preview1" "fd_seek"
                (func $seek (param i32 i64 i32 i32) (result i32)))
            (import "wasi_snapshot_preview1" "fd_read"
                (func $read (param i32 i32 i32 i32) (result i32)))
            (import "wasi_snapshot_preview1" "environ_sizes_get"
                (func $environ (param i32 i32) (result i32)))
            (import "wasi_snapshot_preview1" "fd_write"
                (func $write (param i32 i32 i32 i32) (result i32)))
            (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
            (memory (export "memory") 1)
            (data (i32.const 8) "[\"nothing\"]")
            (data (i32.const 32) "[\"something\"]")
            (data (i32.const 500) "\58\02\00\00\05\00\00\00")
            (func $nothing (result i32)
                (i64.store (i32.const 100) (i64.const 7))
                (i32.store (i32.const 404) (i32.const 7))
                (i32.store (i32.const 408) (i32.const 7))
                (i32.store (i32.const 540) (i32.const 7))
                (i32.and (i32.and (i32.and
                    (i32.and
                        (i32.eqz (i32.or
                            (call $clock (i32.const 0) (i64.const 1) (i32.const 100))
                            (call $clock (i32.const 1) (i64.const 1) (i32.const 108))))
                        (i32.and
                            (i64.eq (i64.load (i32.const 100)) (i64.const 1001000000))
                            (i64.eq (i64.load (i32.const 108)) (i64.const 1002000000))))
                    (i32.and
                        (i32.eqz (i32.or
                            (call $random (i32.const 200) (i32.const 8))
                            (call $random (i32.const 208) (i32.const 8))))
                        (i64.eq (i64.load (i32.const 200)) (i64.load (i32.const 208)))))
                    (i32.and
                        (i32.eq (call $stat (i32.const 3) (i32.const 300)) (i32.const 8))
                        (i32.and
                            (i32.eq
                                (call $seek (i32.const 0) (i64.const 0) (i32.const 0) (i32.const 304))
                                (i32.const 70))
                            (i32.and
                                (i32.eqz (call $read
                                    (i32.const 0) (i32.const 500) (i32.const 1) (i32.const 540)))
                                (i32.eqz (i32.load (i32.const 540)))))))
                    (i32.and
                        (i32.and
                            (i32.eqz (call $environ (i32.const 404) (i32.const 408)))
                            (i32.eqz (i32.or
                                (i32.load (i32.const 404))
                                (i32.load (i32.const 408)))))
                        (i32.and
                            (i32.eqz (call $write
                                (i32.const 1) (i32.const 500) (i32.const 1) (i32.const 520)))
                            (i32.eq (i32.load (i32.const 520)) (i32.const 5))))))
            (func (export "crucible_inputs") (result i64)
                (if (result i64) (call $nothing)
                    (then (i64.or (i64.shl (i64.const 8) (i64.const 32)) (i64.const 11)))
                    (else (i64.or (i64.shl (i64.const 32) (i64.const 32)) (i64.const 13)))))
            (func (export "crucible_alloc") (param i32) (result i32) (i32.const 1024))
            (func (export "crucible_view") (param i32 i32) (result i64)
                (call $exit (i32.const 3))
                (i64.const 0)))"#;
        let plugin = Loaded::Wasm(Module::engine(probe.as_bytes(), SMALL).unwrap());
        assert_eq!(plugin.inputs().unwrap(), ["nothing"]);
        let Err(Fault::Trap(why)) = plugin.view(&Request::default()) else {
            panic!("an exit stops the plugin");
        };
        assert!(why.contains("exit status 3"), "{why}");
    }

    #[test]
    fn a_plugin_that_never_returns_runs_out_of_fuel() {
        let plugin = load("[]", "(loop (br 0))");
        assert_eq!(plugin.view(&Request::default()), Err(Fault::Fuel));
    }

    #[test]
    fn a_plugin_that_grows_past_its_memory_is_stopped() {
        let plugin = load("[]", "(drop (memory.grow (i32.const 64)))");
        assert_eq!(plugin.view(&Request::default()), Err(Fault::Memory));
        let big = r#"(module (memory (export "memory") 64))"#;
        let plugin = Loaded::Wasm(Module::load(big.as_bytes(), SMALL).unwrap());
        let Err(Fault::Trap(why)) = plugin.inputs() else {
            panic!("64 pages is over the limit");
        };
        assert!(why.contains("memory"), "{why}");
    }

    #[test]
    fn a_plugin_that_grows_past_its_table_limit_is_stopped() {
        let grows = |by: u32| {
            let wat = format!(
                r#"(module
                    (memory (export "memory") 1)
                    (table 1 funcref)
                    (data (i32.const 8) "[]")
                    (func (export "crucible_inputs") (result i64)
                        (drop (table.grow (ref.null func) (i32.const {by})))
                        (i64.or (i64.shl (i64.const 8) (i64.const 32)) (i64.const 2))))"#
            );
            Loaded::Wasm(Module::load(wat.as_bytes(), SMALL).unwrap()).inputs()
        };
        assert_eq!(grows(1023), Ok(vec![]));
        assert_eq!(grows(1024), Err(Fault::Memory));
        let big = r#"(module (memory (export "memory") 1) (table 2000 funcref))"#;
        let plugin = Loaded::Wasm(Module::load(big.as_bytes(), SMALL).unwrap());
        let Err(Fault::Trap(why)) = plugin.inputs() else {
            panic!("2000 entries is over the limit");
        };
        assert!(why.contains("table"), "{why}");
        let many: String = (0..9).map(|_| "(table 1 funcref)").collect();
        let wat = format!(r#"(module (memory (export "memory") 1) {many})"#);
        let plugin = Loaded::Wasm(Module::load(wat.as_bytes(), SMALL).unwrap());
        assert!(matches!(plugin.inputs(), Err(Fault::Trap(_))));
    }

    #[test]
    fn a_host_call_costs_fuel_for_what_it_moves_and_refuses_a_long_list() {
        let calling = |call: &str| {
            let wat = format!(
                r#"(module
                    (import "wasi_snapshot_preview1" "random_get"
                        (func $random (param i32 i32) (result i32)))
                    (import "wasi_snapshot_preview1" "fd_write"
                        (func $write (param i32 i32 i32 i32) (result i32)))
                    (import "wasi_snapshot_preview1" "fd_read"
                        (func $read (param i32 i32 i32 i32) (result i32)))
                    (memory (export "memory") 1)
                    (data (i32.const 8) "[]")
                    (data (i32.const 16) "[\"refused\"]")
                    (data (i32.const 500) "\00\00\00\00\ff\ff\ff\ff")
                    (func (export "crucible_inputs") (result i64)
                        (if (result i64) (i32.eq {call} (i32.const 28))
                            (then (i64.or (i64.shl (i64.const 16) (i64.const 32)) (i64.const 11)))
                            (else (i64.or (i64.shl (i64.const 8) (i64.const 32)) (i64.const 2))))))"#
            );
            Loaded::Wasm(Module::engine(wat.as_bytes(), SMALL).unwrap()).inputs()
        };
        assert_eq!(
            calling("(call $random (i32.const 1024) (i32.const 0x7fffffff))"),
            Err(Fault::Fuel)
        );
        assert_eq!(
            calling("(call $random (i32.const 1024) (i32.const 4096))"),
            Ok(vec![])
        );
        for call in ["$write (i32.const 2)", "$read (i32.const 0)"] {
            let listing = |count: &str| {
                calling(&format!(
                    "(call {call} (i32.const 500) (i32.const {count}) (i32.const 600))"
                ))
            };
            assert_eq!(listing("1"), Ok(vec![]), "{call}");
            assert_eq!(listing("1025"), Ok(vec!["refused".to_string()]), "{call}");
            assert_eq!(listing("-1"), Ok(vec!["refused".to_string()]), "{call}");
        }
    }

    #[test]
    fn a_write_is_kept_whole_up_to_the_reply_limit() {
        let writes = r#"(module
            (import "wasi_snapshot_preview1" "fd_write"
                (func $write (param i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 32)
            (data (i32.const 0) "\00\00\01\00\00\00\18\00")
            (func (export "run") (result i32)
                (drop (call $write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16)))
                (i32.load (i32.const 16))))"#;
        let wrote = |reply_bytes: usize| {
            let limits = Limits {
                reply_bytes,
                ..SMALL
            };
            let mut call = Module::engine(writes.as_bytes(), limits)
                .unwrap()
                .start()
                .unwrap();
            let run = call
                .instance
                .get_typed_func::<(), u32>(&call.store, "run")
                .unwrap();
            let reported = run.call(&mut call.store, ()).unwrap();
            let held = call.store.data();
            (reported, held.output.len(), held.overflowed)
        };
        assert_eq!(wrote(2 << 20), (0x18_0000, 0x18_0000, false));
        assert_eq!(wrote(0x18_0000), (0x18_0000, 0x18_0000, false));
        assert_eq!(wrote(4096), (0x18_0000, 4096, true));
    }

    #[test]
    fn a_trap_or_an_error_reply_is_reported() {
        let plugin = load("[]", "(unreachable)");
        assert!(matches!(
            plugin.view(&Request::default()),
            Err(Fault::Trap(_))
        ));
        let plugin = load(r#"{"error":"no verdicts"}"#, "");
        assert_eq!(
            plugin.view(&Request::default()),
            Err(Fault::Refused("no verdicts".into()))
        );
    }

    #[test]
    fn a_reply_that_is_too_long_or_out_of_bounds_is_refused() {
        let far = r#"(module
            (memory (export "memory") 1)
            (func (export "crucible_inputs") (result i64)
                (i64.or (i64.shl (i64.const 65530) (i64.const 32)) (i64.const 64))))"#;
        let plugin = Loaded::Wasm(Module::load(far.as_bytes(), SMALL).unwrap());
        assert!(matches!(plugin.inputs(), Err(Fault::Reply(_))));
        let long = r#"(module
            (memory (export "memory") 1)
            (func (export "crucible_inputs") (result i64) (i64.const 5000)))"#;
        let plugin = Loaded::Wasm(Module::load(long.as_bytes(), SMALL).unwrap());
        let Err(Fault::Reply(why)) = plugin.inputs() else {
            panic!("5000 bytes is over the limit");
        };
        assert!(why.contains("5000 bytes"), "{why}");
    }

    #[test]
    fn inputs_outside_a_task_s_files_are_refused() {
        for path in ["../session.jsonl", "/etc/passwd", "a/../../b", ""] {
            let plugin = load(&serde_json::to_string(&[path]).unwrap(), "");
            assert_eq!(plugin.inputs(), Err(Fault::Input(path.into())), "{path}");
        }
        let plugin = load(r#"["a","b","c"]"#, "");
        assert_eq!(plugin.inputs(), Err(Fault::Inputs(2)));
    }

    #[test]
    fn a_view_the_catalog_refuses_is_a_fault() {
        let script = r#"{"view":{"title":"T","body":{"script":{"src":"x.js"}}}}"#;
        assert!(matches!(
            load(script, "").view(&Request::default()),
            Err(Fault::Reply(_))
        ));
        let tabs = r#"{"view":{"title":"T","body":{"tabs":{"id":"t","tabs":[]}}}}"#;
        assert_eq!(
            load(tabs, "").view(&Request::default()),
            Err(Fault::View(crucible_view::Problem::NoTabs("t".into())))
        );
    }

    #[test]
    fn the_report_example_s_plugin_merges_both_lanes() {
        let pack = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/report");
        let wasm = std::fs::read(pack.join("ui/report.wasm"))
            .expect("build it with `just example-report-view`");
        let plugin = Loaded::Wasm(Module::load(&wasm, Limits::DEFAULT).unwrap());
        assert_eq!(plugin.inputs().unwrap(), ["verdicts.json"]);
        let file = |lane: &str| File {
            path: "verdicts.json".into(),
            task: format!("triage-{lane}"),
            text: std::fs::read_to_string(pack.join(format!("inbox/{lane}.json"))).unwrap(),
        };
        let triaged = |params: &[(&str, crucible_view::Param)]| {
            let view = plugin
                .view(&Request {
                    files: vec![file("go"), file("python")],
                    params: params
                        .iter()
                        .map(|(id, value)| (id.to_string(), value.clone()))
                        .collect(),
                })
                .unwrap();
            let Node::Stack { children } = &view.body else {
                panic!("the report is a stack");
            };
            let Node::Stats { items } = &children[0] else {
                panic!("the report starts with its stats");
            };
            let triaged = items[0].value.clone();
            (view, triaged)
        };
        let (view, all) = triaged(&[]);
        assert_eq!((view.title.as_str(), all.as_str()), ("Triage report", "64"));
        assert_eq!(
            view.body.counts().into_keys().collect::<Vec<_>>(),
            [
                "bars",
                "canvas",
                "columns",
                "flow",
                "grid",
                "heatmap",
                "histogram",
                "mermaid",
                "meter",
                "plot",
                "section",
                "stack",
                "stats",
                "table",
                "tabs"
            ]
        );
        let select = crucible_view::Param::Select("triage-go".into());
        assert_eq!(triaged(&[("task", select)]).1, "34");
        let bad = plugin.view(&Request {
            params: Default::default(),
            files: vec![File {
                path: "verdicts.json".into(),
                task: "triage-go".into(),
                text: "{".into(),
            }],
        });
        let Err(Fault::Refused(why)) = bad else {
            panic!("a torn file is the plugin's error");
        };
        assert!(why.starts_with("verdicts.json from triage-go:"), "{why}");
    }
}

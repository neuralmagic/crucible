//! Runs a pack's view plugin: a WebAssembly module interpreted under a fuel and memory limit,
//! whose only imports may be the WASI calls [`wasi`] answers with nothing. Every call gets a
//! fresh instance.

mod js;
mod wasi;

pub use crate::plugin::js::{JsEngine, Script};

use std::path::{Component, Path};

use crucible_view::{ALLOC, INPUTS, Reply, Request, VIEW, View};
use wasmi::{
    Config, Engine, Instance, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder,
    TrapCode,
};

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
    pub view: crucible_view::Limits,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        fuel: 100_000_000_000,
        memory_bytes: 256 << 20,
        table_elements: 1 << 20,
        reply_bytes: 64 << 20,
        inputs: 16,
        view: crucible_view::Limits::DEFAULT,
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

/// A view plugin of either kind: a compiled module, or JavaScript source.
pub enum Loaded {
    Wasm(Plugin),
    Js(Script),
}

impl Loaded {
    /// The captured file paths the plugin reads.
    pub fn inputs(&self) -> Result<Vec<String>, Fault> {
        match self {
            Loaded::Wasm(plugin) => plugin.inputs(),
            Loaded::Js(script) => script.inputs(),
        }
    }

    /// The plugin's view of `request`, checked against the view limits.
    pub fn view(&self, request: &Request) -> Result<View, Fault> {
        match self {
            Loaded::Wasm(plugin) => plugin.view(request),
            Loaded::Js(script) => script.view(request),
        }
    }
}

pub struct Plugin {
    engine: Engine,
    module: Module,
    limits: Limits,
}

/// One call's limits, what the plugin reads and has written, and its clock.
struct Held {
    limits: StoreLimits,
    /// The plugin's standard input, and how much of it has been read.
    input: Vec<u8>,
    taken: usize,
    /// The plugin's standard output, up to `output_limit` bytes; `overflowed` past that.
    output: Vec<u8>,
    output_limit: usize,
    overflowed: bool,
    /// The start of the plugin's standard error.
    said: Vec<u8>,
    clock: u64,
}

impl wasi::Guest for Held {
    fn tick(&mut self) -> u64 {
        self.clock += 1_000_000;
        self.clock
    }

    fn room(&self, fd: i32) -> usize {
        if fd == 1 {
            self.output_limit.saturating_sub(self.output.len())
        } else {
            SAID.saturating_sub(self.said.len())
        }
    }

    fn write(&mut self, fd: i32, bytes: &[u8], whole: usize) {
        let kept = &bytes[..bytes.len().min(self.room(fd))];
        if fd == 1 {
            self.overflowed |= whole > kept.len();
            self.output.extend_from_slice(kept);
        } else {
            self.said.extend_from_slice(kept);
        }
    }

    fn unread(&self) -> usize {
        self.input.len().saturating_sub(self.taken)
    }

    fn read(&mut self, buffer: &mut [u8]) -> usize {
        let rest = &self.input[self.taken.min(self.input.len())..];
        let filled = rest.len().min(buffer.len());
        buffer[..filled].copy_from_slice(&rest[..filled]);
        self.taken += filled;
        filled
    }
}

/// The most of a plugin's output a fault quotes.
const SAID: usize = 2048;
/// The most tables one plugin may have.
const TABLES: usize = 8;

struct Call {
    store: Store<Held>,
    instance: Instance,
    memory: Memory,
}

impl Plugin {
    pub fn load(wasm: &[u8], limits: Limits) -> Result<Plugin, Fault> {
        let (engine, module) = module(wasm)?;
        Ok(Plugin {
            engine,
            module,
            limits,
        })
    }

    /// The captured file paths the plugin reads.
    pub fn inputs(&self) -> Result<Vec<String>, Fault> {
        let mut call = self.call()?;
        let ask = call
            .instance
            .get_typed_func::<(), u64>(&call.store, INPUTS)
            .map_err(|_| Fault::Export(INPUTS))?;
        let packed = ask
            .call(&mut call.store, ())
            .map_err(|e| said(fault(e), &call.store))?;
        let paths: Vec<String> = serde_json::from_slice(&call.read(packed, &self.limits)?)
            .map_err(|e| Fault::Reply(e.to_string()))?;
        asked_for(paths, &self.limits)
    }

    /// The plugin's view of `request`, checked against the view limits.
    pub fn view(&self, request: &Request) -> Result<View, Fault> {
        let mut call = self.call()?;
        let body = serde_json::to_vec(request).map_err(|e| Fault::Reply(e.to_string()))?;
        let len = u32::try_from(body.len()).map_err(|_| Fault::Memory)?;
        let alloc = call
            .instance
            .get_typed_func::<u32, u32>(&call.store, ALLOC)
            .map_err(|_| Fault::Export(ALLOC))?;
        let build = call
            .instance
            .get_typed_func::<(u32, u32), u64>(&call.store, VIEW)
            .map_err(|_| Fault::Export(VIEW))?;
        let at = alloc
            .call(&mut call.store, len)
            .map_err(|e| said(fault(e), &call.store))?;
        call.memory
            .write(&mut call.store, at as usize, &body)
            .map_err(|e| Fault::Trap(e.to_string()))?;
        let packed = build
            .call(&mut call.store, (at, len))
            .map_err(|e| said(fault(e), &call.store))?;
        let reply: Reply = serde_json::from_slice(&call.read(packed, &self.limits)?)
            .map_err(|e| Fault::Reply(e.to_string()))?;
        match reply {
            Reply::View(view) => {
                view.check(&self.limits.view)?;
                Ok(view)
            }
            Reply::Error(error) => Err(Fault::Refused(error)),
        }
    }

    fn call(&self) -> Result<Call, Fault> {
        start(&self.engine, &self.module, &self.limits)
    }
}

/// `wasm` as a module whose only imports are WASI calls the host answers.
fn module(wasm: &[u8]) -> Result<(Engine, Module), Fault> {
    let mut config = Config::default();
    config.consume_fuel(true).wasm_simd(true);
    let engine = Engine::new(&config);
    let module = Module::new(&engine, wasm).map_err(|e| Fault::Module(e.to_string()))?;
    let foreign = module
        .imports()
        .find(|import| import.module() != wasi::MODULE || !wasi::offers(import.name()));
    match foreign {
        Some(import) => Err(Fault::Import(format!(
            "{}.{}",
            import.module(),
            import.name()
        ))),
        None => Ok((engine, module)),
    }
}

/// A fresh instance of `module` under `limits`, with nothing on its standard input.
fn start(engine: &Engine, module: &Module, limits: &Limits) -> Result<Call, Fault> {
    let store_limits = StoreLimitsBuilder::new()
        .memory_size(limits.memory_bytes)
        .memories(1)
        .table_elements(limits.table_elements)
        .tables(TABLES)
        .instances(1)
        .trap_on_grow_failure(true)
        .build();
    let mut store = Store::new(
        engine,
        Held {
            limits: store_limits,
            input: Vec::new(),
            taken: 0,
            output: Vec::new(),
            output_limit: limits.reply_bytes,
            overflowed: false,
            said: Vec::new(),
            clock: 1_000_000_000,
        },
    );
    store.limiter(|held| &mut held.limits);
    store
        .set_fuel(limits.fuel)
        .map_err(|e| Fault::Module(e.to_string()))?;
    let mut linker = Linker::new(engine);
    wasi::link(&mut linker).map_err(|e| Fault::Module(e.to_string()))?;
    let instance = linker
        .instantiate_and_start(&mut store, module)
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

/// The paths a plugin asked for, when each is inside a task's captured files and there are not
/// too many.
fn asked_for(paths: Vec<String>, limits: &Limits) -> Result<Vec<String>, Fault> {
    if paths.len() > limits.inputs {
        return Err(Fault::Inputs(limits.inputs));
    }
    match paths.iter().find(|path| !captured(path)) {
        Some(path) => Err(Fault::Input(path.clone())),
        None => Ok(paths),
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
    use crucible_view::{File, Node, PlotKind, Request};

    use crate::plugin::{Fault, Limits, Plugin};

    const SMALL: Limits = Limits {
        fuel: 5_000_000,
        memory_bytes: 2 << 20,
        table_elements: 1024,
        reply_bytes: 4096,
        inputs: 2,
        view: crucible_view::Limits::DEFAULT,
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

    fn load(reply: &str, body: &str) -> Plugin {
        Plugin::load(module(reply, body).as_bytes(), SMALL).unwrap()
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

    #[test]
    fn the_request_is_written_where_the_plugin_allocated() {
        let echo = r#"(module
            (memory (export "memory") 1)
            (func (export "crucible_alloc") (param i32) (result i32) (i32.const 1024))
            (func (export "crucible_view") (param i32 i32) (result i64)
                (i64.or (i64.shl (i64.extend_i32_u (local.get 0)) (i64.const 32))
                        (i64.extend_i32_u (local.get 1)))))"#;
        let plugin = Plugin::load(echo.as_bytes(), SMALL).unwrap();
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

    #[test]
    fn only_the_wasi_calls_the_host_answers_may_be_imported() {
        let importing = |module: &str, name: &str| {
            let wat = format!(
                r#"(module
                    (import "{module}" "{name}" (func (param i32 i32) (result i32)))
                    (memory (export "memory") 1))"#
            );
            Plugin::load(wat.as_bytes(), SMALL).err()
        };
        for (module, name) in [
            ("wasi_snapshot_preview1", "path_open"),
            ("wasi_snapshot_preview1", "sock_send"),
            ("wasi_snapshot_preview1", "fd_readdir"),
            ("wasi_snapshot_preview1", "fd_prestat_get"),
            ("env", "fetch"),
            ("env", "random_get"),
        ] {
            assert_eq!(
                importing(module, name),
                Some(Fault::Import(format!("{module}.{name}"))),
            );
        }
        assert_eq!(importing("wasi_snapshot_preview1", "random_get"), None);
        assert!(matches!(
            Plugin::load(b"not wasm", SMALL),
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
        let plugin = Plugin::load(probe.as_bytes(), SMALL).unwrap();
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
        let plugin = Plugin::load(big.as_bytes(), SMALL).unwrap();
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
            Plugin::load(wat.as_bytes(), SMALL).unwrap().inputs()
        };
        assert_eq!(grows(1023), Ok(vec![]));
        assert_eq!(grows(1024), Err(Fault::Memory));
        let big = r#"(module (memory (export "memory") 1) (table 2000 funcref))"#;
        let plugin = Plugin::load(big.as_bytes(), SMALL).unwrap();
        let Err(Fault::Trap(why)) = plugin.inputs() else {
            panic!("2000 entries is over the limit");
        };
        assert!(why.contains("table"), "{why}");
        let many: String = (0..9).map(|_| "(table 1 funcref)").collect();
        let wat = format!(r#"(module (memory (export "memory") 1) {many})"#);
        let plugin = Plugin::load(wat.as_bytes(), SMALL).unwrap();
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
            Plugin::load(wat.as_bytes(), SMALL).unwrap().inputs()
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
            let (engine, module) = crate::plugin::module(writes.as_bytes()).unwrap();
            let mut call = crate::plugin::start(&engine, &module, &limits).unwrap();
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
        let plugin = Plugin::load(far.as_bytes(), SMALL).unwrap();
        assert!(matches!(plugin.inputs(), Err(Fault::Reply(_))));
        let long = r#"(module
            (memory (export "memory") 1)
            (func (export "crucible_inputs") (result i64) (i64.const 5000)))"#;
        let plugin = Plugin::load(long.as_bytes(), SMALL).unwrap();
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
        let plugin = Plugin::load(&wasm, Limits::DEFAULT).unwrap();
        assert_eq!(plugin.inputs().unwrap(), ["verdicts.json"]);
        let file = |lane: &str| File {
            path: "verdicts.json".into(),
            task: format!("triage-{lane}"),
            text: std::fs::read_to_string(pack.join(format!("inbox/{lane}.json"))).unwrap(),
        };
        let view = plugin
            .view(&Request {
                params: Default::default(),
                files: vec![file("go"), file("python")],
            })
            .unwrap();
        assert_eq!(view.title, "Triage report");
        let Node::Stack { children } = &view.body else {
            panic!("the report is a stack");
        };
        let [Node::Stats { items }, Node::Tabs { tabs, .. }] = children.as_slice() else {
            panic!("the report is its stats over its tabs");
        };
        assert_eq!(
            (items[0].label.as_str(), items[0].value.as_str()),
            ("Triaged", "64")
        );
        let all = tabs.iter().find(|tab| tab.label == "All").unwrap();
        assert_eq!(all.count, Some(64));
        let by_category: u64 = tabs.iter().skip(4).filter_map(|tab| tab.count).sum();
        assert_eq!(by_category, 64);
        let Node::Stack { children: summary } = &tabs[0].body else {
            panic!("Summary is a stack");
        };
        let Node::Bars { bars } = &summary[0] else {
            panic!("Summary starts with its bars");
        };
        let labels: Vec<&str> = bars.iter().map(|bar| bar.label.as_str()).collect();
        assert_eq!(labels, ["3.3", "3.4", "3.5"]);
        let charted: f64 = bars
            .iter()
            .flat_map(|bar| &bar.segments)
            .map(|segment| segment.value)
            .sum();
        assert_eq!(charted, 64.0);
        let Node::Stack { children: charts } = &tabs[1].body else {
            panic!("Charts is a stack");
        };
        let [
            Node::Columns { children: side },
            Node::Flow { nodes, links },
            Node::Columns { children: under },
        ] = charts.as_slice()
        else {
            panic!("Charts is two plots side by side over a flow over two more charts");
        };
        let [
            Node::Plot {
                kind: PlotKind::Line,
                series: lines,
                categories,
                x_scale: crucible_view::Scale::Linear,
                ..
            },
            Node::Plot {
                kind: PlotKind::Scatter,
                series: marks,
                ..
            },
        ] = side.as_slice()
        else {
            panic!("the plots are a line plot and a scatter plot");
        };
        assert_eq!(categories, &["3.3", "3.4", "3.5"]);
        assert!(
            lines
                .iter()
                .flat_map(|series| &series.points)
                .all(|point| [0.0, 1.0, 2.0].contains(&point[0])),
            "x indexes a release"
        );
        let [
            Node::Histogram {
                values: ages,
                bins: Some(8),
                ..
            },
            Node::Heatmap {
                x: heat_x,
                y: heat_y,
                values: heat,
                ..
            },
        ] = under.as_slice()
        else {
            panic!("under the flow are a histogram and a heatmap");
        };
        assert_eq!(ages.len(), 64);
        assert_eq!(heat_x, categories);
        assert_eq!(heat_y.len(), 6);
        assert_eq!(heat.iter().flatten().sum::<f64>(), 64.0);
        assert!(
            nodes
                .iter()
                .filter(|node| node.id.starts_with("basis:"))
                .all(|node| node.tone == crucible_view::Tone::Neutral),
            "the bases take the palette's colours"
        );
        let columns = crucible_view::flow_columns(nodes, links).unwrap();
        let reaching = |column: usize| -> f64 {
            links
                .iter()
                .filter(|link| {
                    nodes
                        .iter()
                        .zip(&columns)
                        .any(|(node, at)| node.id == link.to && *at == column)
                })
                .map(|link| link.value)
                .sum()
        };
        assert_eq!(columns.iter().max(), Some(&2));
        assert_eq!((reaching(1), reaching(2)), (64.0, 64.0));
        assert_eq!(
            nodes
                .iter()
                .filter(|node| node.id.starts_with("lane:"))
                .count(),
            2
        );
        assert!(lines.iter().all(|series| series.points.len() == 3));
        let lined: f64 = lines.iter().flat_map(|s| &s.points).map(|p| p[1]).sum();
        assert_eq!(lined, 64.0);
        let marked: usize = marks.iter().map(|series| series.points.len()).sum();
        assert_eq!(marked, 64);
        let Node::Canvas {
            width, commands, ..
        } = &tabs[2].body
        else {
            panic!("Canvas is a canvas");
        };
        assert_eq!(*width, 130.0 + 64.0 * 3.0 + 190.0);
        let filled = commands
            .iter()
            .filter(|command| matches!(command, crucible_view::Draw::FillRect(_)))
            .count();
        assert_eq!(filled, 18, "a cell per category and release");
        let cells: usize = commands
            .iter()
            .filter_map(|command| match command {
                crucible_view::Draw::FillText { text, .. } => text.parse::<usize>().ok(),
                _ => None,
            })
            .sum();
        assert_eq!(cells, 64);
        assert!(commands.iter().any(|command| matches!(
            command,
            crucible_view::Draw::FillText { text, .. } if text == "70%"
        )));
        let Node::Table { rows, columns, .. } = &all.body else {
            panic!("All is a table");
        };
        assert_eq!(rows.len(), 64);
        assert_eq!(columns[0].id, "ticket");
        let row = rows.iter().find(|row| row.id == "DEMO-101").unwrap();
        assert_eq!(
            row.cells[0].href.as_deref(),
            Some("https://issues.example.com/browse/DEMO-101")
        );
        assert_eq!(
            (row.cells[11].text.as_str(), row.cells[11].href.as_deref()),
            ("triage-go", Some("run:task/triage-go"))
        );
        assert_eq!(
            (columns[4].id.as_str(), columns[4].kind, columns[5].kind),
            (
                "cvss",
                crucible_view::ColumnKind::Number,
                crucible_view::ColumnKind::Number
            )
        );
        assert_eq!(
            (row.cells[4].text.as_str(), row.cells[4].value),
            ("6.1", None)
        );
        assert_eq!(
            (row.cells[5].text.as_str(), row.cells[5].value),
            ("57 d", Some(57.0))
        );
        let detail = row.detail.as_deref().unwrap();
        assert!(
            detail.starts_with("### DEMO-101: sbom_inaccurate\n"),
            "{detail}"
        );
        for part in [
            "The final-artifact package scan for the image reported absent.",
            "| Field | Value |\n|---|---|\n| Package | `google.golang.org/grpc 1.64.0` |",
            "[triage-go](run:task/triage-go)",
            "[its verdicts](run:file/triage-go/verdicts.json)",
        ] {
            assert!(detail.contains(part), "{part} in {detail}");
        }
        let [_, _, Node::Section { id, open, body, .. }] = summary.as_slice() else {
            panic!("Summary ends with how a ticket is categorised");
        };
        assert_eq!((id.as_str(), *open), ("categorised", false));
        let Node::Mermaid { source } = body.as_ref() else {
            panic!("the folded section is a diagram");
        };
        assert!(source.starts_with("flowchart LR\n"), "{source}");
        let [
            crucible_view::Control::Select {
                id, options, value, ..
            },
        ] = view.controls.as_slice()
        else {
            panic!("the report has one select");
        };
        let values: Vec<&str> = options.iter().map(|o| o.value.as_str()).collect();
        assert_eq!(
            (id.as_str(), value.as_str(), values),
            ("task", "all", vec!["all", "triage-go", "triage-python"])
        );
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
            let crucible_view::Control::Select { value, .. } = &view.controls[0] else {
                panic!("the report has one select");
            };
            (items[0].value.clone(), value.clone())
        };
        let select = |value: &str| crucible_view::Param::Select(value.into());
        assert_eq!(
            triaged(&[("task", select("triage-go"))]),
            ("34".into(), "triage-go".into())
        );
        assert_eq!(
            triaged(&[("task", select("triage-python"))]),
            ("30".into(), "triage-python".into())
        );
        assert_eq!(
            triaged(&[("task", select("triage-rust"))]),
            ("64".into(), "all".into())
        );
        assert_eq!(
            triaged(&[("task", crucible_view::Param::Toggle(true))]),
            ("64".into(), "all".into())
        );
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

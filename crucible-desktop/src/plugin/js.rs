//! Runs a view plugin written in JavaScript. The source is compiled and run by Javy's QuickJS
//! engine, a WebAssembly module the host instantiates under the same limits as any plugin, so
//! the script gets nothing the engine module does not: no files, network or real clock.

use std::sync::Arc;

use crucible_view::{Request, View};
use serde::Deserialize;
use wasmi::{Engine, Module};

use crate::plugin::{Fault, Limits, asked_for, fault, module, said, start};

/// What the host puts after a script: the canvas it can draw with and the `Cell` helpers.
const PRELUDE: &str = include_str!("../../../crucible-view/js/prelude.js");
/// What the host puts last: reading what is asked and writing the answer.
const EPILOGUE: &str = include_str!("../../../crucible-view/js/epilogue.js");

/// The byte the epilogue writes before the script's answer, after anything the script printed.
const ANSWER: u8 = 0x1e;

/// Javy's QuickJS engine module, ready to instantiate.
pub struct JsEngine {
    engine: Engine,
    module: Module,
    limits: Limits,
}

impl JsEngine {
    pub fn load(wasm: &[u8], limits: Limits) -> Result<JsEngine, Fault> {
        let (engine, module) = module(wasm)?;
        Ok(JsEngine {
            engine,
            module,
            limits,
        })
    }
}

/// A JavaScript view plugin: a module that defines `inputs` and `view`.
pub struct Script {
    engine: Arc<JsEngine>,
    source: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Answer {
    Inputs(Vec<String>),
    View(View),
    Error(String),
}

impl Script {
    pub fn new(engine: Arc<JsEngine>, source: &str) -> Script {
        Script {
            engine,
            source: format!("{source}\n{PRELUDE}\n{EPILOGUE}"),
        }
    }

    /// The captured file paths the script reads.
    pub fn inputs(&self) -> Result<Vec<String>, Fault> {
        match self.ask(b"{}".to_vec())? {
            Answer::Inputs(paths) => asked_for(paths, &self.engine.limits),
            Answer::View(_) => Err(Fault::Reply("a view, when asked for inputs".into())),
            Answer::Error(error) => Err(Fault::Refused(error)),
        }
    }

    /// The script's view of `request`, checked against the view limits.
    pub fn view(&self, request: &Request) -> Result<View, Fault> {
        let asked = serde_json::to_vec(&serde_json::json!({ "view": request }))
            .map_err(|e| Fault::Reply(e.to_string()))?;
        match self.ask(asked)? {
            Answer::View(view) => {
                view.check(&self.engine.limits.view)?;
                Ok(view)
            }
            Answer::Inputs(_) => Err(Fault::Reply("inputs, when asked for a view".into())),
            Answer::Error(error) => Err(Fault::Refused(error)),
        }
    }

    /// Compiles the script in a fresh engine, runs it with `asked` on its standard input, and
    /// reads its answer from its standard output.
    fn ask(&self, asked: Vec<u8>) -> Result<Answer, Fault> {
        let JsEngine {
            engine,
            module,
            limits,
        } = &*self.engine;
        let mut call = start(engine, module, limits)?;
        let export = |call: &crate::plugin::Call, name: &'static str| {
            call.instance
                .get_func(&call.store, name)
                .ok_or(Fault::Export(name))
        };
        let initialize = export(&call, "initialize-runtime")?
            .typed::<(), ()>(&call.store)
            .map_err(|_| Fault::Export("initialize-runtime"))?;
        let allocate = export(&call, "cabi_realloc")?
            .typed::<(u32, u32, u32, u32), u32>(&call.store)
            .map_err(|_| Fault::Export("cabi_realloc"))?;
        let compile = export(&call, "compile-src")?
            .typed::<(u32, u32), u32>(&call.store)
            .map_err(|_| Fault::Export("compile-src"))?;
        let invoke = export(&call, "invoke")?
            .typed::<(u32, u32, u32, u32, u32), ()>(&call.store)
            .map_err(|_| Fault::Export("invoke"))?;

        initialize
            .call(&mut call.store, ())
            .map_err(|e| said(fault(e), &call.store))?;

        let source = self.source.as_bytes();
        let len = u32::try_from(source.len()).map_err(|_| Fault::Memory)?;
        let at = allocate
            .call(&mut call.store, (0, 0, 1, len))
            .map_err(|e| said(fault(e), &call.store))?;
        call.memory
            .write(&mut call.store, at as usize, source)
            .map_err(|e| Fault::Trap(e.to_string()))?;
        let result = compile
            .call(&mut call.store, (at, len))
            .map_err(|e| said(fault(e), &call.store))?;
        let mut area = [0; 12];
        call.memory
            .read(&call.store, result as usize, &mut area)
            .map_err(|e| Fault::Reply(e.to_string()))?;
        let word = |i: usize| u32::from_le_bytes([area[i], area[i + 1], area[i + 2], area[i + 3]]);
        let (failed, code, code_len) = (word(0) != 0, word(4), word(8));
        if failed {
            let mut message = vec![0; (code_len as usize).min(4096)];
            call.memory
                .read(&call.store, code as usize, &mut message)
                .map_err(|e| Fault::Reply(e.to_string()))?;
            return Err(Fault::Script(
                String::from_utf8_lossy(&message).trim().to_string(),
            ));
        }

        let held = call.store.data_mut();
        held.input = asked;
        held.taken = 0;
        invoke
            .call(&mut call.store, (code, code_len, 0, 0, 0))
            .map_err(|e| said(fault(e), &call.store))?;
        let held = call.store.data();
        if held.overflowed {
            return Err(Fault::Reply(format!(
                "over the {} bytes allowed",
                limits.reply_bytes
            )));
        }
        let Some(at) = held.output.iter().rposition(|byte| *byte == ANSWER) else {
            return Err(Fault::Reply("the script wrote no answer".into()));
        };
        serde_json::from_slice(&held.output[at + 1..]).map_err(|e| Fault::Reply(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crucible_view::{Draw, File, Node, Request};

    use crate::plugin::{Fault, JsEngine, Limits, Script};

    fn engine() -> Arc<JsEngine> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/js-engine/plugin.wasm");
        let wasm = std::fs::read(path).expect("fetch it with `just desktop-js-engine`");
        Arc::new(JsEngine::load(&wasm, Limits::DEFAULT).unwrap())
    }

    const COUNTS: &str = r#"
        export const inputs = ["verdicts.json"];
        export function view({ files }) {
            const rows = files.flatMap((file) => JSON.parse(file.text));
            const canvas = new Canvas(120, 40);
            const ctx = canvas.getContext("2d");
            ctx.fillStyle = "bad";
            ctx.globalAlpha = 0.5;
            ctx.fillRect(0, 0, rows.length, 10);
            ctx.textAlign = "center";
            ctx.fillText(`${rows.length} rows`, 60, 30);
            return {
                title: "Counts",
                body: { stack: { children: [
                    { stats: { items: [{ label: "Rows", value: String(rows.length), tone: "good" }] } },
                    canvas.node(),
                ] } },
            };
        }
    "#;

    fn request(text: &str) -> Request {
        Request {
            params: Default::default(),
            files: vec![File {
                path: "verdicts.json".into(),
                task: "triage".into(),
                text: text.into(),
            }],
        }
    }

    #[test]
    #[ignore = "needs assets/js-engine/plugin.wasm from `just desktop-js-engine`"]
    fn a_script_names_its_inputs_and_draws_a_view_with_the_canvas() {
        let script = Script::new(engine(), COUNTS);
        let started = std::time::Instant::now();
        assert_eq!(script.inputs().unwrap(), ["verdicts.json"]);
        let view = script.view(&request("[1, 2, 3]")).unwrap();
        eprintln!("two calls took {:?}", started.elapsed());
        assert_eq!(view.title, "Counts");
        let Node::Stack { children } = &view.body else {
            panic!("the view is a stack");
        };
        let [
            Node::Stats { items },
            Node::Canvas {
                width, commands, ..
            },
        ] = children.as_slice()
        else {
            panic!("the view is its stats over its canvas");
        };
        assert_eq!(items[0].value, "3");
        assert_eq!(*width, 120.0);
        assert_eq!(commands.len(), 5);
        assert_eq!(commands[2], Draw::FillRect([0.0, 0.0, 3.0, 10.0]));
        assert!(matches!(&commands[4], Draw::FillText { text, .. } if text == "3 rows"));
    }

    #[test]
    #[ignore = "needs assets/js-engine/plugin.wasm from `just desktop-js-engine`"]
    fn the_canvas_records_transforms_baselines_widths_and_hints_under_the_browser_s_names() {
        let script = Script::new(
            engine(),
            r#"
            export const inputs = [];
            export function view() {
                const canvas = new Canvas(100, 50);
                const ctx = canvas.getContext("2d");
                ctx.save();
                ctx.translate(10, 20);
                ctx.scale(2, 3);
                ctx.rotate(0.5);
                ctx.transform(1, 0, 0, 1, 4, 5);
                ctx.setTransform(1, 0, 0, 1, 6, 7);
                ctx.setTransform();
                ctx.resetTransform();
                ctx.textBaseline = "hanging";
                const hanging = ctx.textBaseline;
                ctx.textBaseline = "ideographic";
                ctx.restore();
                ctx.fillText("plain", 1, 2);
                ctx.fillText("cut", 1, 2, 30);
                ctx.hint(0, 0, 10, 10, 42);
                return { title: `${hanging} ${ctx.textBaseline}`, body: canvas.node() };
            }
            "#,
        );
        let view = script.view(&Request::default()).unwrap();
        assert_eq!(
            view.title, "hanging alphabetic",
            "restore brings the baseline back"
        );
        let Node::Canvas { commands, .. } = &view.body else {
            panic!("the view is a canvas");
        };
        let text = |text: &str, max_width: Option<f32>| Draw::FillText {
            text: text.into(),
            x: 1.0,
            y: 2.0,
            max_width,
        };
        assert_eq!(
            commands.as_slice(),
            [
                Draw::Save,
                Draw::Translate([10.0, 20.0]),
                Draw::Scale([2.0, 3.0]),
                Draw::Rotate(0.5),
                Draw::Transform([1.0, 0.0, 0.0, 1.0, 4.0, 5.0]),
                Draw::SetTransform([1.0, 0.0, 0.0, 1.0, 6.0, 7.0]),
                Draw::ResetTransform,
                Draw::ResetTransform,
                Draw::TextBaseline(crucible_view::TextBaseline::Top),
                Draw::TextBaseline(crucible_view::TextBaseline::Bottom),
                Draw::Restore,
                text("plain", None),
                text("cut", Some(30.0)),
                Draw::Hint {
                    x: 0.0,
                    y: 0.0,
                    width: 10.0,
                    height: 10.0,
                    text: "42".into(),
                },
            ]
        );
    }

    #[test]
    #[ignore = "needs assets/js-engine/plugin.wasm from `just desktop-js-engine`"]
    fn a_script_that_does_not_compile_or_throws_says_why() {
        let broken = Script::new(engine(), "export const inputs = [;");
        let Err(Fault::Script(why)) = broken.inputs() else {
            panic!("a syntax error is a compile failure");
        };
        assert!(
            why.contains("SyntaxError") || why.contains("unexpected"),
            "{why}"
        );

        let throws = Script::new(engine(), COUNTS);
        let Err(Fault::Refused(why)) = throws.view(&request("not json")) else {
            panic!("a thrown error is the script's answer");
        };
        assert!(why.starts_with("SyntaxError"), "{why}");
        assert!(why.contains("at view (function.mjs:4:"), "{why}");

        let missing = Script::new(engine(), "export const inputs = [];");
        let Err(Fault::Refused(why)) = missing.view(&Request::default()) else {
            panic!("a script with no view function cannot answer");
        };
        assert!(why.contains("view"), "{why}");
    }

    #[test]
    #[ignore = "needs assets/js-engine/plugin.wasm from `just desktop-js-engine`"]
    fn a_script_is_held_to_the_same_limits_and_rules() {
        let wasm = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/js-engine/plugin.wasm"),
        )
        .unwrap();
        let tight = Limits {
            fuel: 400_000_000,
            ..Limits::DEFAULT
        };
        let spins = Script::new(
            Arc::new(JsEngine::load(&wasm, tight).unwrap()),
            "export const inputs = []; export function view() { for (;;) {} }",
        );
        assert_eq!(spins.view(&Request::default()), Err(Fault::Fuel));

        let outside = Script::new(engine(), r#"export const inputs = ["../session.jsonl"];"#);
        assert_eq!(
            outside.inputs(),
            Err(Fault::Input("../session.jsonl".into()))
        );

        let unknown = Script::new(
            engine(),
            r#"export const inputs = [];
               export function view() { return { title: "T", body: { script: { src: "x.js" } } }; }"#,
        );
        assert!(matches!(
            unknown.view(&Request::default()),
            Err(Fault::Reply(_))
        ));

        let chatty = Script::new(
            engine(),
            r#"console.log("starting");
               export const inputs = ["a.json"];
               export function view() {
                   console.log("drawing");
                   return { title: "T", body: { text: { text: "hi" } } };
               }"#,
        );
        assert_eq!(chatty.inputs().unwrap(), ["a.json"]);
        assert_eq!(chatty.view(&Request::default()).unwrap().title, "T");

        let reaches = Script::new(
            engine(),
            r#"export const inputs = [];
               export function view() {
                   return { title: String(typeof fetch) + typeof require + typeof process,
                            body: { text: { text: String(Date.now()) } } };
               }"#,
        );
        let view = reaches.view(&Request::default()).unwrap();
        assert_eq!(view.title, "undefinedundefinedundefined");
    }

    #[test]
    #[ignore = "needs assets/js-engine/plugin.wasm from `just desktop-js-engine`"]
    fn a_reply_over_a_mebibyte_arrives_whole() {
        let source = r#"export const inputs = [];
            export function view() {
                const rows = [];
                for (let i = 0; i < 70; i++) {
                    rows.push({ id: String(i), cells: [{ text: "x".repeat(20000) }] });
                }
                return { title: "T", body: { table: {
                    id: "t", columns: [{ id: "c", title: "C" }], rows } } };
            }"#;
        let script = Script::new(engine(), source);
        let view = script.view(&Request::default()).unwrap();
        let Node::Table { rows, .. } = &view.body else {
            panic!("the view is a table");
        };
        assert_eq!(rows.len(), 70);
        assert_eq!(rows[69].cells[0].text.len(), 20_000);

        let wasm = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/js-engine/plugin.wasm"),
        )
        .unwrap();
        let small = Limits {
            reply_bytes: 1 << 20,
            ..Limits::DEFAULT
        };
        let capped = Script::new(Arc::new(JsEngine::load(&wasm, small).unwrap()), source);
        assert_eq!(
            capped.view(&Request::default()),
            Err(Fault::Reply("over the 1048576 bytes allowed".into()))
        );
    }

    #[test]
    #[ignore = "needs assets/js-engine/plugin.wasm from `just desktop-js-engine`"]
    fn a_script_s_formatted_cells_match_the_rust_constructors() {
        use crucible_view::Cell;
        let cases = include_str!("../../../crucible-view/js/formatted.json");
        let script = Script::new(
            engine(),
            &format!(
                r#"export const inputs = [];
                   const CASES = {cases};
                   const OFF = [NaN, Infinity, -Infinity, 1.1e15, -1.1e15, Number.MAX_VALUE, "5", undefined, null];
                   export function view() {{
                       const rows = CASES.map(([helper, input], i) => ({{
                           id: `case-${{i}}`, cells: [Cell[helper](input)] }}));
                       for (const helper of ["duration", "bytes", "percent", "count", "date"]) {{
                           OFF.forEach((input, i) => rows.push({{
                               id: `off-${{helper}}-${{i}}`, cells: [Cell[helper](input)] }}));
                       }}
                       rows.push({{ id: "date-before", cells: [Cell.date(-62135596801)] }});
                       rows.push({{ id: "date-after", cells: [Cell.date(253370764801)] }});
                       return {{ title: typeof Cell + Object.isFrozen(Cell), body: {{ table: {{
                           id: "t", columns: [{{ id: "c", title: "C" }}], rows }} }} }};
                   }}"#
            ),
        );
        let view = script.view(&Request::default()).unwrap();
        assert_eq!(view.title, "objecttrue");
        let Node::Table { rows, .. } = &view.body else {
            panic!("the view is a table");
        };
        let cases: Vec<(String, f64, String)> = serde_json::from_str(cases).unwrap();
        let rust = |helper: &str, input: f64| match helper {
            "duration" => Cell::duration(input),
            "bytes" => Cell::bytes(input),
            "percent" => Cell::percent(input),
            "count" => Cell::count(input),
            "date" => Cell::date(input),
            other => panic!("no helper {other}"),
        };
        for (i, (helper, input, text)) in cases.iter().enumerate() {
            let row = &rows[i];
            assert_eq!(row.id, format!("case-{i}"));
            assert_eq!(row.cells[0].text, *text, "{helper}({input}) in JavaScript");
            assert_eq!(row.cells[0], rust(helper, *input), "{helper}({input})");
        }
        let off = &rows[cases.len()..];
        assert_eq!(off.len(), 5 * 9 + 2);
        for row in off {
            assert_eq!(row.cells[0], Cell::default(), "{}", row.id);
        }
    }

    #[test]
    #[ignore = "needs assets/js-engine/plugin.wasm from `just desktop-js-engine`"]
    fn a_script_reads_params_as_plain_values_and_declares_controls() {
        use crucible_view::{Control, Param};
        let script = Script::new(
            engine(),
            r#"export const inputs = [];
               export function view({ files, params }) {
                   return {
                       title: JSON.stringify(params) + typeof params.min + files.length,
                       body: { text: { text: "hi" } },
                       controls: [
                           { select: { id: "lane", label: "Lane", value: params.lane ?? "all",
                               options: [{ value: "all", label: "All" }, { value: "go", label: "Go" }] } },
                           { toggle: { id: "open", label: "Open", on: params.open ?? false } },
                           { range: { id: "min", label: "Min", min: 0, max: 10, step: 0.5,
                               value: params.min ?? 0 } },
                       ],
                   };
               }"#,
        );
        let bare = script.view(&Request::default()).unwrap();
        assert_eq!(bare.title, "{}undefined0");
        assert_eq!(
            bare.controls,
            [
                Control::select("lane", "Lane", &[("all", "All"), ("go", "Go")], "all"),
                Control::toggle("open", "Open", false),
                Control::range("min", "Min", [0.0, 10.0], 0.5, 0.0),
            ]
        );
        let asked = Request {
            params: [
                ("lane".to_string(), Param::Select("go".into())),
                ("open".to_string(), Param::Toggle(true)),
                ("min".to_string(), Param::Range(7.5)),
            ]
            .into(),
            ..request("[]")
        };
        let view = script.view(&asked).unwrap();
        assert_eq!(view.title, r#"{"lane":"go","min":7.5,"open":true}number1"#);
        assert_eq!(
            view.controls[0],
            Control::select("lane", "Lane", &[("all", "All"), ("go", "Go")], "go")
        );
        assert_eq!(
            view.controls[2],
            Control::range("min", "Min", [0.0, 10.0], 0.5, 7.5)
        );

        let twice = Script::new(
            engine(),
            r#"export const inputs = [];
               export function view() {
                   return { title: "T", body: { text: { text: "hi" } }, controls: [
                       { toggle: { id: "a", label: "A", on: true } },
                       { toggle: { id: "a", label: "B", on: false } } ] };
               }"#,
        );
        assert_eq!(
            twice.view(&Request::default()),
            Err(Fault::View(crucible_view::Problem::DuplicateControl(
                "a".into()
            )))
        );
    }
}

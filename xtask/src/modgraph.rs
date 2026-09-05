//! `cargo xtask modgraph [--check] [--json] [SRC_ROOT]`
//!
//! Item-level dependency graph of one crate's source tree. Every `.rs` file is a module
//! (`a/b.rs` is `a::b`, `a/mod.rs` is `a`, `main.rs`/`lib.rs` are `root`). Each cross-module
//! reference is resolved through the file's `use` aliases to the item it names, including paths
//! inside macro bodies. References made from a `#[cfg(test)]` module are tagged `test` and
//! ignored by the cycle check.
//!
//! `--check` fails when any set of modules depends on itself (a strongly connected component of
//! more than one module). `--json` prints the raw graph for further analysis.

use proc_macro2::{TokenStream, TokenTree};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use syn::visit::Visit;

#[derive(serde::Serialize)]
struct Item {
    name: String,
    kind: &'static str,
    vis: String,
}

#[derive(serde::Serialize)]
struct Module {
    file: String,
    lines: usize,
    items: Vec<Item>,
}

#[derive(serde::Serialize, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
struct Edge {
    from: String,
    to: String,
    item: String,
    test: bool,
}

#[derive(serde::Serialize)]
struct Graph {
    modules: BTreeMap<String, Module>,
    edges: Vec<(Edge, usize)>,
}

pub fn run(args: &[String]) -> Result<bool, String> {
    let mut check = false;
    let mut json = false;
    let mut root = None;
    for a in args {
        match a.as_str() {
            "--check" => check = true,
            "--json" => json = true,
            other if other.starts_with('-') => return Err(format!("unknown flag {other}")),
            other => root = Some(PathBuf::from(other)),
        }
    }
    let root = root.unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("crucible")
            .join("src")
    });
    let graph = extract(&root)?;
    if json {
        let out = serde_json::to_string_pretty(&graph)
            .map_err(|e| format!("serializing the graph: {e}"))?;
        println!("{out}");
        return Ok(true);
    }
    let cycles = report(&graph);
    Ok(!check || cycles.is_empty())
}

fn module_of(root: &Path, file: &Path) -> Result<String, String> {
    let rel = file
        .strip_prefix(root)
        .map_err(|e| format!("{} is outside {}: {e}", file.display(), root.display()))?
        .with_extension("");
    let mut parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if matches!(
        parts.last().map(String::as_str),
        Some("mod" | "main" | "lib")
    ) {
        parts.pop();
    }
    Ok(if parts.is_empty() {
        "root".into()
    } else {
        parts.join("::")
    })
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|e| format!("reading {}: {e}", dir.display()))?
            .path();
        if path.is_dir() {
            walk(&path, out)?;
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

fn item_name(i: &syn::Item) -> Option<(String, &'static str, &syn::Visibility)> {
    Some(match i {
        syn::Item::Fn(f) => (f.sig.ident.to_string(), "fn", &f.vis),
        syn::Item::Struct(s) => (s.ident.to_string(), "struct", &s.vis),
        syn::Item::Enum(e) => (e.ident.to_string(), "enum", &e.vis),
        syn::Item::Trait(t) => (t.ident.to_string(), "trait", &t.vis),
        syn::Item::Type(t) => (t.ident.to_string(), "type", &t.vis),
        syn::Item::Const(c) => (c.ident.to_string(), "const", &c.vis),
        syn::Item::Static(s) => (s.ident.to_string(), "static", &s.vis),
        syn::Item::Mod(m) => (m.ident.to_string(), "mod", &m.vis),
        _ => return None,
    })
}

fn vis(v: &syn::Visibility) -> String {
    match v {
        syn::Visibility::Public(_) => "pub".into(),
        syn::Visibility::Restricted(r) => format!("pub({})", path_string(&r.path)),
        syn::Visibility::Inherited => "private".into(),
    }
}

fn path_string(p: &syn::Path) -> String {
    p.segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && match &a.meta {
                syn::Meta::List(l) => l.tokens.to_string().contains("test"),
                _ => false,
            }
    })
}

struct Collector<'a> {
    module: String,
    modules: &'a BTreeSet<String>,
    root_items: &'a BTreeSet<String>,
    aliases: HashMap<String, Vec<String>>,
    in_test: bool,
    depth: usize,
    edges: HashMap<Edge, usize>,
    items: Vec<Item>,
}

impl Collector<'_> {
    fn resolve(&self, segs: &[String]) -> Option<Vec<String>> {
        let first = segs.first()?.as_str();
        let own = || {
            self.module
                .split("::")
                .map(String::from)
                .collect::<Vec<_>>()
        };
        let mut full = match first {
            "crate" | "crucible" => segs[1..].to_vec(),
            "self" => {
                let mut v = own();
                v.extend_from_slice(&segs[1..]);
                v
            }
            "super" => {
                let mut v = own();
                v.pop();
                v.extend_from_slice(&segs[1..]);
                v
            }
            _ => {
                let mut v = self.aliases.get(first)?.clone();
                v.extend_from_slice(&segs[1..]);
                v
            }
        };
        if self.module == "root" {
            full.retain(|s| s != "root");
        }
        Some(full)
    }

    fn bump(&mut self, to: String, item: String) {
        if to == self.module {
            return;
        }
        let edge = Edge {
            from: self.module.clone(),
            to,
            item,
            test: self.in_test,
        };
        *self.edges.entry(edge).or_insert(0) += 1;
    }

    fn record(&mut self, segs: &[String]) {
        let Some(full) = self.resolve(segs) else {
            return;
        };
        let best = (1..=full.len())
            .filter(|&n| self.modules.contains(&full[..n].join("::")))
            .max();
        match best {
            Some(n) => {
                let item = full.get(n).cloned().unwrap_or_else(|| "<module>".into());
                self.bump(full[..n].join("::"), item);
            }
            None => {
                if let Some(head) = full.first().filter(|h| self.root_items.contains(*h)) {
                    self.bump("root".into(), head.clone());
                }
            }
        }
    }

    fn use_tree(&mut self, tree: &syn::UseTree, mut prefix: Vec<String>) {
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                self.use_tree(&p.tree, prefix);
            }
            syn::UseTree::Name(n) => {
                let name = n.ident.to_string();
                if name != "self" {
                    prefix.push(name.clone());
                }
                self.record(&prefix);
                if let Some(r) = self.resolve(&prefix) {
                    self.aliases
                        .insert(prefix.last().cloned().unwrap_or(name), r);
                }
            }
            syn::UseTree::Rename(n) => {
                prefix.push(n.ident.to_string());
                self.record(&prefix);
                if let Some(r) = self.resolve(&prefix) {
                    self.aliases.insert(n.rename.to_string(), r);
                }
            }
            syn::UseTree::Glob(_) => {
                prefix.push("*".into());
                self.record(&prefix);
            }
            syn::UseTree::Group(g) => {
                for t in &g.items {
                    self.use_tree(t, prefix.clone());
                }
            }
        }
    }

    /// Macro bodies are opaque to syn; scan their tokens for `a::b::c` sequences.
    fn scan_tokens(&mut self, ts: TokenStream) {
        let trees: Vec<TokenTree> = ts.into_iter().collect();
        let is_colon =
            |t: Option<&TokenTree>| matches!(t, Some(TokenTree::Punct(p)) if p.as_char() == ':');
        let mut i = 0;
        while i < trees.len() {
            match &trees[i] {
                TokenTree::Group(g) => self.scan_tokens(g.stream()),
                TokenTree::Ident(id) => {
                    let mut segs = vec![id.to_string()];
                    let mut j = i + 1;
                    while is_colon(trees.get(j)) && is_colon(trees.get(j + 1)) {
                        match trees.get(j + 2) {
                            Some(TokenTree::Ident(n)) => {
                                segs.push(n.to_string());
                                j += 3;
                            }
                            _ => break,
                        }
                    }
                    if segs.len() > 1 {
                        self.record(&segs);
                    }
                    i = j;
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
    }
}

impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        self.use_tree(&i.tree, Vec::new());
    }

    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        let was_test = self.in_test;
        self.in_test |= is_cfg_test(&i.attrs);
        if i.content.is_some() {
            let saved = self.aliases.clone();
            self.depth += 1;
            syn::visit::visit_item_mod(self, i);
            self.depth -= 1;
            self.aliases = saved;
        }
        self.in_test = was_test;
    }

    fn visit_path(&mut self, p: &'ast syn::Path) {
        let segs: Vec<String> = p.segments.iter().map(|s| s.ident.to_string()).collect();
        if segs.len() > 1 {
            self.record(&segs);
        }
        syn::visit::visit_path(self, p);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        self.scan_tokens(m.tokens.clone());
        syn::visit::visit_macro(self, m);
    }

    fn visit_item(&mut self, i: &'ast syn::Item) {
        if self.depth == 0
            && !self.in_test
            && let Some((name, kind, v)) = item_name(i)
        {
            self.items.push(Item {
                name,
                kind,
                vis: vis(v),
            });
        }
        syn::visit::visit_item(self, i);
    }
}

fn parse(file: &Path) -> Result<(String, syn::File), String> {
    let src =
        std::fs::read_to_string(file).map_err(|e| format!("reading {}: {e}", file.display()))?;
    let ast = syn::parse_file(&src).map_err(|e| format!("parsing {}: {e}", file.display()))?;
    Ok((src, ast))
}

fn extract(root: &Path) -> Result<Graph, String> {
    let mut files = Vec::new();
    walk(root, &mut files)?;
    files.sort();
    let mut modules = BTreeSet::new();
    let mut root_items = BTreeSet::new();
    for f in &files {
        let m = module_of(root, f)?;
        if m == "root" {
            let (_, ast) = parse(f)?;
            root_items.extend(ast.items.iter().filter_map(item_name).map(|(n, _, _)| n));
        }
        modules.insert(m);
    }
    let mut graph = Graph {
        modules: BTreeMap::new(),
        edges: Vec::new(),
    };
    let mut edges: HashMap<Edge, usize> = HashMap::new();
    for f in &files {
        let (src, ast) = parse(f)?;
        let module = module_of(root, f)?;
        let mut c = Collector {
            module: module.clone(),
            modules: &modules,
            root_items: &root_items,
            aliases: HashMap::new(),
            in_test: false,
            depth: 0,
            edges: HashMap::new(),
            items: Vec::new(),
        };
        c.visit_file(&ast);
        for (e, n) in c.edges {
            *edges.entry(e).or_insert(0) += n;
        }
        let file = f
            .strip_prefix(root)
            .map_err(|e| format!("{}: {e}", f.display()))?
            .display()
            .to_string();
        graph.modules.insert(
            module,
            Module {
                file,
                lines: src.lines().count(),
                items: c.items,
            },
        );
    }
    graph.edges = edges.into_iter().collect();
    graph.edges.sort();
    Ok(graph)
}

/// Tarjan's algorithm over the non-test module graph; components of more than one module.
fn cycles(adj: &BTreeMap<&str, BTreeSet<&str>>) -> Vec<Vec<String>> {
    struct State<'a> {
        index: HashMap<&'a str, usize>,
        low: HashMap<&'a str, usize>,
        stack: Vec<&'a str>,
        on_stack: BTreeSet<&'a str>,
        next: usize,
        out: Vec<Vec<String>>,
    }
    fn visit<'a>(v: &'a str, adj: &BTreeMap<&'a str, BTreeSet<&'a str>>, st: &mut State<'a>) {
        st.index.insert(v, st.next);
        st.low.insert(v, st.next);
        st.next += 1;
        st.stack.push(v);
        st.on_stack.insert(v);
        for &w in adj.get(v).into_iter().flatten() {
            if !st.index.contains_key(w) {
                visit(w, adj, st);
                let lw = st.low[w];
                if let Some(x) = st.low.get_mut(v) {
                    *x = (*x).min(lw);
                }
            } else if st.on_stack.contains(w) {
                let iw = st.index[w];
                if let Some(x) = st.low.get_mut(v) {
                    *x = (*x).min(iw);
                }
            }
        }
        if st.low[v] == st.index[v] {
            let mut comp = Vec::new();
            while let Some(w) = st.stack.pop() {
                st.on_stack.remove(w);
                comp.push(w.to_string());
                if w == v {
                    break;
                }
            }
            if comp.len() > 1 {
                comp.sort();
                st.out.push(comp);
            }
        }
    }
    let mut st = State {
        index: HashMap::new(),
        low: HashMap::new(),
        stack: Vec::new(),
        on_stack: BTreeSet::new(),
        next: 0,
        out: Vec::new(),
    };
    for &v in adj.keys() {
        if !st.index.contains_key(v) {
            visit(v, adj, &mut st);
        }
    }
    st.out.sort_by_key(|c| std::cmp::Reverse(c.len()));
    st.out
}

fn report(graph: &Graph) -> Vec<Vec<String>> {
    let mut adj: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut items: BTreeMap<(&str, &str), BTreeSet<&str>> = BTreeMap::new();
    for m in graph.modules.keys() {
        adj.entry(m).or_default();
    }
    for (e, _) in graph.edges.iter().filter(|(e, _)| !e.test) {
        adj.entry(&e.from).or_default().insert(&e.to);
        items.entry((&e.from, &e.to)).or_default().insert(&e.item);
    }
    let pairs = items.len();
    println!(
        "{} modules, {} dependency pairs (non-test)",
        graph.modules.len(),
        pairs
    );
    let comps = cycles(&adj);
    println!("\n{} cyclic component(s)", comps.len());
    for comp in &comps {
        println!("  [{}] {}", comp.len(), comp.join(" "));
        for a in comp {
            for b in comp {
                if let Some(its) = items.get(&(a.as_str(), b.as_str())) {
                    let list: Vec<&str> = its.iter().copied().collect();
                    println!("      {a} -> {b}: {}", list.join(", "));
                }
            }
        }
    }
    let mut fan_in: BTreeMap<&str, usize> = BTreeMap::new();
    let mut fan_out: BTreeMap<&str, usize> = BTreeMap::new();
    for (a, b) in items.keys() {
        *fan_out.entry(a).or_default() += 1;
        *fan_in.entry(b).or_default() += 1;
    }
    println!("\nmost depended on");
    for (m, n) in top(&fan_in) {
        println!("  {n:3}  {m}");
    }
    println!("\nmost dependencies");
    for (m, n) in top(&fan_out) {
        println!("  {n:3}  {m}");
    }
    let mut homonyms: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();
    for (m, info) in &graph.modules {
        for it in info.items.iter().filter(|i| i.kind != "mod") {
            homonyms.entry((&it.name, it.kind)).or_default().push(m);
        }
    }
    homonyms.retain(|_, mods| mods.len() > 1);
    println!(
        "\n{} item name(s) defined in more than one module",
        homonyms.len()
    );
    for ((name, kind), mods) in &homonyms {
        println!("  {name} ({kind}): {}", mods.join(", "));
    }
    comps
}

fn top(m: &BTreeMap<&str, usize>) -> Vec<(String, usize)> {
    let mut v: Vec<(String, usize)> = m.iter().map(|(k, n)| (k.to_string(), *n)).collect();
    v.sort_by_key(|(k, n)| (std::cmp::Reverse(*n), k.clone()));
    v.truncate(10);
    v
}

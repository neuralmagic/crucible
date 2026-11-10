//! Rewrites of deprecated arguments into the values that replace them.

use std::ops::Range;

use crate::docs::Reference;
use crate::index::{Arg, Call, Index, ValueKind};

/// One quick fix: the edits that replace or remove what a diagnostic covers on one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewrite {
    pub title: String,
    /// The diagnostics' spans this covers.
    pub at: Vec<Range<usize>>,
    pub edits: Vec<(Range<usize>, String)>,
}

pub fn rewrites(index: &Index, source: &str, reference: &Reference) -> Vec<Rewrite> {
    index
        .calls
        .iter()
        .filter(|call| {
            reference.function(&call.callee).is_some()
                && index.resolve(&call.callee, call.span.start).is_none()
        })
        .flat_map(|call| {
            let site = Site { call, source };
            [
                site.fanout(),
                site.revise(),
                site.bound(),
                site.when(),
                site.decider(),
            ]
            .into_iter()
            .flatten()
            .chain(site.implied(index))
        })
        .collect()
}

struct Site<'a> {
    call: &'a Call,
    source: &'a str,
}

impl Site<'_> {
    fn arg(&self, name: &str) -> Option<(usize, &Arg)> {
        self.call
            .args
            .iter()
            .enumerate()
            .find(|(_, arg)| arg.name.as_ref().is_some_and(|(n, _)| n == name))
    }

    fn text(&self, arg: &Arg) -> &str {
        &self.source[arg.value.span.clone()]
    }

    fn start(&self, index: usize) -> usize {
        let arg = &self.call.args[index];
        arg.name
            .as_ref()
            .map_or(arg.value.span.start, |(_, at)| at.start)
    }

    fn end(&self, index: usize) -> usize {
        self.call.args[index].value.span.end
    }

    fn name_span(&self, index: usize) -> Range<usize> {
        self.call.args[index]
            .name
            .as_ref()
            .map_or(self.start(index)..self.start(index), |(_, at)| at.clone())
    }

    /// Deletions that drop `removed` and the separators that belonged to them.
    fn removal(&self, mut removed: Vec<usize>) -> Vec<(Range<usize>, String)> {
        removed.sort_unstable();
        let mut edits = Vec::new();
        let mut run = removed.into_iter().peekable();
        while let Some(first) = run.next() {
            let mut last = first;
            while run.peek() == Some(&(last + 1)) {
                last += 1;
                run.next();
            }
            let range = if last + 1 < self.call.args.len() {
                self.start(first)..self.start(last + 1)
            } else if first > 0 {
                self.end(first - 1)..self.end(last)
            } else {
                self.start(first)..self.end(last)
            };
            edits.push((range, String::new()));
        }
        edits
    }

    fn rewrite(
        &self,
        title: &str,
        replaced: (Range<usize>, String),
        removed: Vec<usize>,
        deprecated: &[usize],
    ) -> Rewrite {
        let mut edits = vec![replaced];
        edits.extend(self.removal(removed));
        Rewrite {
            title: format!("Rewrite as `{title}`"),
            at: deprecated.iter().map(|i| self.name_span(*i)).collect(),
            edits,
        }
    }

    /// One removal per `depends_on` entry another argument already implies.
    fn implied(&self, index: &Index) -> Vec<Rewrite> {
        let Some((argument, depends_on)) = self.arg("depends_on") else {
            return Vec::new();
        };
        let ValueKind::List(entries) = &depends_on.value.kind else {
            return Vec::new();
        };
        index
            .implied_entries(self.call)
            .into_iter()
            .map(|(position, name, by)| {
                let entry = entries[position].span.clone();
                let edits = if entries.len() == 1 {
                    self.removal(vec![argument])
                } else if position + 1 < entries.len() {
                    vec![(entry.start..entries[position + 1].span.start, String::new())]
                } else {
                    vec![(entries[position - 1].span.end..entry.end, String::new())]
                };
                Rewrite {
                    title: format!("Remove `{name}`: `{by}` already names it"),
                    at: vec![entry],
                    edits,
                }
            })
            .collect()
    }

    fn fanout(&self) -> Option<Rewrite> {
        let (_, over) = self.arg("over")?;
        let (width_at, width) = self.arg("max_fanout")?;
        if matches!(over.value.kind, ValueKind::Call(_)) {
            return None;
        }
        let value = format!("each({}, max = {})", self.text(over), self.text(width));
        Some(self.rewrite(
            "over = each(..., max = N)",
            (over.value.span.clone(), value),
            vec![width_at],
            &[width_at],
        ))
    }

    fn revise(&self) -> Option<Rewrite> {
        let (_, tasks) = self.arg("revise")?;
        let (rounds_at, rounds) = self.arg("max_rounds")?;
        if matches!(tasks.value.kind, ValueKind::Call(_)) {
            return None;
        }
        let value = format!(
            "send_back({}, rounds = {})",
            self.text(tasks),
            self.text(rounds)
        );
        Some(self.rewrite(
            "revise = send_back(..., rounds = N)",
            (tasks.value.span.clone(), value),
            vec![rounds_at],
            &[rounds_at],
        ))
    }

    fn bound(&self) -> Option<Rewrite> {
        if self.call.callee != "evaluate" {
            return None;
        }
        let (_, threshold) = self.arg("threshold")?;
        let (direction_at, direction) = self.arg("direction")?;
        let function = match direction.value.kind.clone() {
            ValueKind::Str(side) if side == "higher" => "at_least",
            ValueKind::Str(side) if side == "lower" => "at_most",
            _ => return None,
        };
        if matches!(threshold.value.kind, ValueKind::Call(_)) {
            return None;
        }
        let value = format!("{function}({})", self.text(threshold));
        Some(self.rewrite(
            &format!("threshold = {function}(n)"),
            (threshold.value.span.clone(), value),
            vec![direction_at],
            &[direction_at],
        ))
    }

    fn when(&self) -> Option<Rewrite> {
        let (_, when) = self.arg("when")?;
        if matches!(when.value.kind, ValueKind::Call(_)) {
            return None;
        }
        if let Some((at, otherwise)) = self.arg("otherwise")
            && otherwise.value.kind == ValueKind::Bool(true)
            && self.arg("answers").is_none()
        {
            let value = format!("{}.otherwise()", self.text(when));
            return Some(self.rewrite(
                "when = question.otherwise()",
                (when.value.span.clone(), value),
                vec![at],
                &[at],
            ));
        }
        let (at, answers) = self.arg("answers")?;
        let labels = match &answers.value.kind {
            ValueKind::Str(_) => self.text(answers).to_owned(),
            ValueKind::List(items) => {
                let first = items.first()?.span.start;
                let last = items.last()?.span.end;
                self.source[first..last].to_owned()
            }
            _ => return None,
        };
        let mut removed = vec![at];
        removed.extend(self.arg("otherwise").map(|(at, _)| at));
        let value = format!("{}.any_of({labels})", self.text(when));
        Some(self.rewrite(
            "when = question.any_of(...)",
            (when.value.span.clone(), value),
            removed.clone(),
            &removed,
        ))
    }

    fn decider(&self) -> Option<Rewrite> {
        if self.call.callee != "route" || self.arg("decider").is_some() {
            return None;
        }
        let set = |name: &str| {
            self.arg(name)
                .filter(|(_, arg)| arg.value.kind != ValueKind::NoneLit)
        };
        let whole = |index: usize| self.start(index)..self.end(index);
        let human = set("human").filter(|(_, arg)| arg.value.kind == ValueKind::Bool(true));
        match (set("min_confidence"), set("source"), human) {
            (Some((at, confidence)), None, None) => {
                let files = set("files");
                let mut value =
                    format!("decider = model(min_confidence = {}", self.text(confidence));
                if let Some((_, files)) = files {
                    value.push_str(&format!(", files = {}", self.text(files)));
                }
                value.push(')');
                let removed: Vec<usize> = files.iter().map(|(at, _)| *at).collect();
                let mut deprecated = vec![at];
                deprecated.extend(&removed);
                Some(self.rewrite(
                    "decider = model(...)",
                    (whole(at), value),
                    removed,
                    &deprecated,
                ))
            }
            (None, Some((at, task)), None) => Some(self.rewrite(
                "decider = output(task)",
                (whole(at), format!("decider = output({})", self.text(task))),
                Vec::new(),
                &[at],
            )),
            (None, None, Some((at, _))) => {
                let knobs: Vec<(usize, String)> = ["review", "timeout"]
                    .into_iter()
                    .filter_map(|name| {
                        let (at, arg) = set(name)?;
                        Some((at, format!("{name} = {}", self.text(arg))))
                    })
                    .collect();
                let inner: Vec<&str> = knobs.iter().map(|(_, text)| text.as_str()).collect();
                let removed: Vec<usize> = knobs.iter().map(|(at, _)| *at).collect();
                let mut deprecated = vec![at];
                deprecated.extend(&removed);
                Some(self.rewrite(
                    "decider = human(...)",
                    (whole(at), format!("decider = human({})", inner.join(", "))),
                    removed,
                    &deprecated,
                ))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crucible::plan::starlark::compile_source_with;

    use crate::check::parse;
    use crate::fix::*;

    const DEPRECATED: &str = r#"scan = command(name = "scan", run = "true", emits = {"items": "list", "q": "boolean"})
author = agent(name = "author", prompt = "p")
gate = route(
    name = "gate",
    min_confidence = 0.8,
    files = [],
    depends_on = [scan],
    questions = {
        "area": choice(ask = "which?", options = ["x", "y", "z"]),
        "go": noul(ask = "go?", drop = ["no", "uncertain"]),
    },
)
a = command(name = "a", run = "true", depends_on = [gate], when = gate.area, answers = ["x", "y"])
b = command(
    name = "b",
    run = "true",
    depends_on = [gate],
    when = gate.area,
    otherwise = True,
)
c = command(name = "c", run = "true", depends_on = [gate], when = gate.go, answers = "yes")
fan = command(name = "fan", run = "true", max_fanout = 4, depends_on = [scan], over = scan.items)
review = evaluate(name = "review", run = "true", depends_on = [author], threshold = 1, direction = "higher", revise = author, max_rounds = 3)
ask = route(name = "ask", human = True, review = "review.md", timeout = "1h", depends_on = [scan], questions = {"ok": noul(ask = "ok?")})
src = route(name = "src", source = scan, depends_on = [scan], questions = {"q": noul(ask = "q?")})
workflow(type = "playbook", tasks = [scan, author, gate, a, b, c, fan, review, ask, src])
"#;

    const REWRITTEN: &str = r#"scan = command(name = "scan", run = "true", emits = {"items": "list", "q": "boolean"})
author = agent(name = "author", prompt = "p")
gate = route(
    name = "gate",
    decider = model(min_confidence = 0.8, files = []),
    depends_on = [scan],
    questions = {
        "area": choice(ask = "which?", options = ["x", "y", "z"]),
        "go": noul(ask = "go?", drop = ["no", "uncertain"]),
    },
)
a = command(name = "a", run = "true", when = gate.area.any_of("x", "y"))
b = command(
    name = "b",
    run = "true",
    when = gate.area.otherwise(),
)
c = command(name = "c", run = "true", when = gate.go.any_of("yes"))
fan = command(name = "fan", run = "true", over = each(scan.items, max = 4))
review = evaluate(name = "review", run = "true", depends_on = [author], threshold = at_least(1), revise = send_back(author, rounds = 3))
ask = route(name = "ask", decider = human(review = "review.md", timeout = "1h"), depends_on = [scan], questions = {"ok": noul(ask = "ok?")})
src = route(name = "src", decider = output(scan), questions = {"q": noul(ask = "q?")})
workflow(type = "playbook", tasks = [scan, author, gate, a, b, c, fan, review, ask, src])
"#;

    fn found(source: &str) -> Vec<Rewrite> {
        let dir = tempfile::tempdir().expect("tempdir");
        let module = parse(source, &dir.path().join("workflow.star")).expect("parses");
        rewrites(&Index::build(&module), source, &Reference::new())
    }

    fn apply(source: &str, rewrite: &Rewrite) -> String {
        let mut edits = rewrite.edits.clone();
        edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        let mut out = source.to_owned();
        for (range, text) in edits {
            out.replace_range(range, &text);
        }
        out
    }

    #[test]
    fn every_deprecated_argument_rewrites_to_the_value_that_replaces_it() {
        let mut source = DEPRECATED.to_owned();
        let mut titles = Vec::new();
        while let Some(rewrite) = found(&source).into_iter().next() {
            titles.push(rewrite.title.clone());
            source = apply(&source, &rewrite);
        }
        assert_eq!(source, REWRITTEN);
        assert_eq!(
            titles,
            [
                "Rewrite as `decider = model(...)`",
                "Rewrite as `when = question.any_of(...)`",
                "Remove `gate`: `when` already names it",
                "Rewrite as `when = question.otherwise()`",
                "Remove `gate`: `when` already names it",
                "Rewrite as `when = question.any_of(...)`",
                "Remove `gate`: `when` already names it",
                "Rewrite as `over = each(..., max = N)`",
                "Remove `scan`: `over` already names it",
                "Rewrite as `revise = send_back(..., rounds = N)`",
                "Rewrite as `threshold = at_least(n)`",
                "Rewrite as `decider = human(...)`",
                "Rewrite as `decider = output(task)`",
                "Remove `scan`: `decider` already names it",
            ]
        );
    }

    #[test]
    fn a_rewritten_pack_compiles_to_the_graph_it_did() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("review.md"), "launch?\n").expect("review");
        let compile = |source: &str| {
            compile_source_with(
                source,
                &dir.path().join("workflow.star"),
                dir.path(),
                &BTreeMap::new(),
            )
            .expect("compiles")
            .canonical_json
        };
        assert_eq!(compile(REWRITTEN), compile(DEPRECATED));
    }

    #[test]
    fn a_rewrite_covers_the_names_it_removes() {
        let rewrites = found(DEPRECATED);
        let covered: Vec<&str> = rewrites
            .iter()
            .flat_map(|rewrite| rewrite.at.iter())
            .map(|at| &DEPRECATED[at.clone()])
            .collect();
        assert_eq!(
            covered,
            [
                "min_confidence",
                "files",
                "answers",
                "gate",
                "otherwise",
                "gate",
                "answers",
                "gate",
                "max_fanout",
                "scan",
                "max_rounds",
                "direction",
                "human",
                "review",
                "timeout",
                "source",
                "scan",
            ]
        );
    }

    #[test]
    fn an_implied_dependency_is_removed_with_its_separator_or_its_argument() {
        for (before, after) in [
            (
                "a = command(name = \"a\", run = \"x\", depends_on = [s, t], over = each(s.items, max = 2))\n",
                "a = command(name = \"a\", run = \"x\", depends_on = [t], over = each(s.items, max = 2))\n",
            ),
            (
                "a = command(name = \"a\", run = \"x\", depends_on = [t, s], over = each(s.items, max = 2))\n",
                "a = command(name = \"a\", run = \"x\", depends_on = [t], over = each(s.items, max = 2))\n",
            ),
            (
                "a = command(\n    name = \"a\",\n    run = \"x\",\n    depends_on = [s],\n    over = each(s.items, max = 2),\n)\n",
                "a = command(\n    name = \"a\",\n    run = \"x\",\n    over = each(s.items, max = 2),\n)\n",
            ),
            (
                "a = command(name = \"a\", run = \"x\", over = each(s.items, max = 2), depends_on = [s])\n",
                "a = command(name = \"a\", run = \"x\", over = each(s.items, max = 2))\n",
            ),
        ] {
            let rewrites = found(before);
            assert_eq!(rewrites.len(), 1, "{before}");
            assert_eq!(rewrites[0].title, "Remove `s`: `over` already names it");
            assert_eq!(apply(before, &rewrites[0]), after);
        }
    }

    #[test]
    fn what_cannot_be_rewritten_safely_is_left_alone() {
        for source in [
            "e = evaluate(name = \"e\", run = \"true\", threshold = 1, direction = side)\n",
            "k = top_k(name = \"k\", k = 1, direction = \"lower\", depends_on = [e])\n",
            "a = command(name = \"a\", run = \"true\", when = g.q, answers = labels)\n",
            "g = route(name = \"g\", source = None, min_confidence = None, questions = {})\n",
            "g = route(name = \"g\", decider = human(), questions = {})\n",
            "a = command(name = \"a\", run = \"true\", over = each(s.items, max = 4))\n",
            "def command(**kw):\n    return kw\nx = command(over = s.items, max_fanout = 4)\n",
            "c = command(name = \"c\", run = \"x\")\na = command(name = \"a\", run = \"x\", depends_on = [c], when = c.ok)\n",
            "a = command(name = \"a\", run = \"x\", depends_on = [optional(g), \"g\"], when = g.q)\n",
            "a = command(name = \"a\", run = \"x\", depends_on = deps, over = each(s.items, max = 2))\n",
        ] {
            assert_eq!(found(source), Vec::new(), "{source}");
        }
    }
}

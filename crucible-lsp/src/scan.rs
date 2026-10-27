//! Bracket, argument, and string structure of a source that may not parse.

use std::ops::Range;

use crucible::plan::starlark::dialect;
use starlark_syntax::codemap::CodeMap;
use starlark_syntax::lexer::{Lexer, Token as LexToken};

use crate::index::bytes;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub open: u8,
    pub start: usize,
    pub end: Option<usize>,
    /// The bare identifier a `(` calls: `agent` in `agent(`.
    pub callee: Option<String>,
    pub parent: Option<usize>,
    pub slots: Vec<Slot>,
    /// `params` in `params = {`: the target of the top-level assignment the bracket opens.
    pub target: Option<String>,
}

/// One argument, list item, or dict entry: the text between two separators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub span: Range<usize>,
    pub key: Option<Key>,
}

/// `name =` in a call or `"name":` in a dict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub name: String,
    pub span: Range<usize>,
    /// Just past the `=` or `:`.
    pub value: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Literal {
    /// Prefix and quotes included.
    pub span: Range<usize>,
    pub content: Range<usize>,
    pub closed: bool,
    pub frame: Option<usize>,
}

#[derive(Debug, Default)]
pub struct Structure {
    pub frames: Vec<Frame>,
    pub strings: Vec<Literal>,
    pub comments: Vec<Range<usize>>,
}

#[derive(Debug, Clone)]
enum Token {
    Ident {
        name: String,
        span: Range<usize>,
        dotted: bool,
    },
    Str {
        content: String,
        span: Range<usize>,
    },
    Other,
}

/// The tokens of the slot or top-level statement being read.
#[derive(Debug, Default)]
struct Run {
    count: usize,
    first: Option<Token>,
    last: Option<Token>,
    target: Option<String>,
}

impl Run {
    fn push(&mut self, token: Token) {
        self.count += 1;
        if self.first.is_none() {
            self.first = Some(token.clone());
        }
        self.last = Some(token);
    }
}

#[derive(Default)]
struct Scanner {
    out: Structure,
    open: Vec<usize>,
    statement: Run,
    /// The current slot of each open frame, parallel to `open`.
    slots: Vec<Run>,
    dotted: bool,
}

/// Fold the compiler lexer's tokens into brackets and slots. An unterminated string becomes an
/// open literal, and the tokens lexed from inside it are skipped.
pub fn scan(source: &str) -> Structure {
    let codemap = CodeMap::new(String::new(), source.to_owned());
    let mut lexer = Lexer::new(source, &dialect(), codemap);
    let mut scanner = Scanner::default();
    let mut skip_until = 0;
    for _ in 0..source.len().saturating_mul(2).saturating_add(16) {
        let Some(lexeme) = lexer.next() else { break };
        match lexeme {
            Ok((start, _, _)) if start < skip_until => {}
            Ok((start, token, end)) => scanner.token(source, token, start..end),
            Err(error) => {
                if let Some(span) = error.into_error().span().map(|at| bytes(at.span))
                    && let Some(content) = string_content(source, &span, false)
                {
                    skip_until = span.end;
                    scanner.string(span, content, String::new(), false);
                }
            }
        }
    }
    scanner.finish(source.len())
}

/// The content of the string literal at `span`, after its prefix and opening quotes.
fn string_content(source: &str, span: &Range<usize>, closed: bool) -> Option<Range<usize>> {
    let text = source.as_bytes().get(span.clone())?;
    let prefix = text
        .iter()
        .take(2)
        .take_while(|b| matches!(b, b'r' | b'R' | b'b' | b'B' | b'f' | b'F'))
        .count();
    let quote = *text.get(prefix).filter(|q| matches!(q, b'"' | b'\''))?;
    let triple = text.get(prefix + 1) == Some(&quote) && text.get(prefix + 2) == Some(&quote);
    let width = if triple { 3 } else { 1 };
    let start = (span.start + prefix + width).min(span.end);
    let end = if closed {
        span.end.saturating_sub(width).max(start)
    } else {
        span.end
    };
    Some(start..end)
}

impl Scanner {
    fn token(&mut self, source: &str, token: LexToken, span: Range<usize>) {
        match token {
            LexToken::Comment(_) => self.out.comments.push(span),
            LexToken::Newline | LexToken::Semicolon if self.open.is_empty() => {
                self.statement = Run::default();
            }
            LexToken::Newline | LexToken::Indent | LexToken::Dedent | LexToken::Tabs => {}
            LexToken::OpeningRound => self.open_frame(b'(', span.start),
            LexToken::OpeningSquare => self.open_frame(b'[', span.start),
            LexToken::OpeningCurly => self.open_frame(b'{', span.start),
            LexToken::ClosingRound => self.close_frame(b'(', span.start),
            LexToken::ClosingSquare => self.close_frame(b'[', span.start),
            LexToken::ClosingCurly => self.close_frame(b'{', span.start),
            LexToken::Comma if !self.open.is_empty() => self.separate(span.start),
            LexToken::Equal => self.assign(span.start),
            LexToken::Colon => self.colon(span.start),
            LexToken::Dot => self.dotted = true,
            LexToken::Identifier(name) => self.ident(name, span),
            LexToken::Load => self.ident("load".to_owned(), span),
            LexToken::String(value) => {
                if let Some(content) = string_content(source, &span, true) {
                    self.string(span, content, value, true);
                }
            }
            LexToken::Bytes(_) => {
                if let Some(content) = string_content(source, &span, true) {
                    self.string(span, content, String::new(), true);
                }
            }
            _ => self.push(Token::Other),
        }
    }

    fn finish(mut self, end: usize) -> Structure {
        while let Some(index) = self.open.pop() {
            if let Some(slot) = self
                .out
                .frames
                .get_mut(index)
                .and_then(|f| f.slots.last_mut())
            {
                slot.span.end = end;
            }
        }
        self.out
    }

    fn run_mut(&mut self) -> &mut Run {
        match self.slots.last_mut() {
            Some(run) => run,
            None => &mut self.statement,
        }
    }

    fn push(&mut self, token: Token) {
        self.dotted = false;
        self.run_mut().push(token);
    }

    fn ident(&mut self, name: String, span: Range<usize>) {
        let dotted = self.dotted;
        self.push(Token::Ident { name, span, dotted });
    }

    fn string(&mut self, span: Range<usize>, content: Range<usize>, value: String, closed: bool) {
        self.out.strings.push(Literal {
            span: span.clone(),
            content,
            closed,
            frame: self.open.last().copied(),
        });
        self.push(Token::Str {
            content: value,
            span,
        });
    }

    fn open_frame(&mut self, open: u8, at: usize) {
        let top_level = self.open.is_empty();
        let run = self.run_mut();
        let callee = match &run.last {
            Some(Token::Ident {
                name,
                dotted: false,
                ..
            }) if open == b'(' => Some(name.clone()),
            _ => None,
        };
        let target = if top_level { run.target.clone() } else { None };
        self.push(Token::Other);
        self.out.frames.push(Frame {
            open,
            start: at,
            end: None,
            callee,
            parent: self.open.last().copied(),
            slots: vec![Slot {
                span: at + 1..at + 1,
                key: None,
            }],
            target,
        });
        self.open.push(self.out.frames.len() - 1);
        self.slots.push(Run::default());
    }

    fn close_frame(&mut self, opener: u8, at: usize) {
        let Some(depth) = self
            .open
            .iter()
            .rposition(|&index| self.out.frames.get(index).is_some_and(|f| f.open == opener))
        else {
            self.push(Token::Other);
            return;
        };
        while self.open.len() > depth {
            let Some(index) = self.open.pop() else { break };
            self.slots.pop();
            if let Some(frame) = self.out.frames.get_mut(index) {
                frame.end = Some(at);
                if let Some(slot) = frame.slots.last_mut() {
                    slot.span.end = at;
                }
            }
        }
        self.dotted = false;
    }

    fn separate(&mut self, at: usize) {
        if let Some(frame) = self.open.last().and_then(|&i| self.out.frames.get_mut(i)) {
            if let Some(slot) = frame.slots.last_mut() {
                slot.span.end = at;
            }
            frame.slots.push(Slot {
                span: at + 1..at + 1,
                key: None,
            });
        }
        *self.run_mut() = Run::default();
        self.dotted = false;
    }

    /// `name =` as the first two tokens of a call slot is a keyword argument; of a top-level
    /// statement, an assignment.
    fn assign(&mut self, at: usize) {
        let run = self.run_mut();
        let key = match (&run.first, run.count) {
            (
                Some(Token::Ident {
                    name,
                    span,
                    dotted: false,
                }),
                1,
            ) => Some((name.clone(), span.clone())),
            _ => None,
        };
        if let Some((name, span)) = key {
            match self.open.last().copied() {
                None => self.statement.target = Some(name),
                Some(index) => {
                    if let Some(frame) = self.out.frames.get_mut(index)
                        && frame.open == b'('
                        && let Some(slot) = frame.slots.last_mut()
                    {
                        slot.key = Some(Key {
                            name,
                            span,
                            value: at + 1,
                        });
                    }
                }
            }
        }
        self.push(Token::Other);
    }

    /// `"name":` as the first two tokens of a dict slot is an entry key.
    fn colon(&mut self, at: usize) {
        let run = self.run_mut();
        let key = match (&run.first, run.count) {
            (Some(Token::Str { content, span }), 1) => Some((content.clone(), span.clone())),
            _ => None,
        };
        if let Some((name, span)) = key
            && let Some(frame) = self.open.last().and_then(|&i| self.out.frames.get_mut(i))
            && frame.open == b'{'
            && let Some(slot) = frame.slots.last_mut()
        {
            slot.key = Some(Key {
                name,
                span,
                value: at + 1,
            });
        }
        self.push(Token::Other);
    }
}

impl Structure {
    pub fn in_comment(&self, offset: usize) -> bool {
        self.comments
            .iter()
            .any(|comment| comment.start < offset && offset <= comment.end)
    }

    /// The string literal whose content holds `offset`, closing quote excluded.
    pub fn string_at(&self, offset: usize) -> Option<&Literal> {
        self.strings
            .iter()
            .find(|literal| literal.content.start <= offset && offset <= literal.content.end)
    }

    /// The innermost bracket pair `offset` sits inside.
    pub fn frame_at(&self, offset: usize) -> Option<usize> {
        self.frames
            .iter()
            .enumerate()
            .filter(|(_, frame)| frame.start < offset && frame.end.is_none_or(|end| offset <= end))
            .max_by_key(|(_, frame)| frame.start)
            .map(|(index, _)| index)
    }

    pub fn frame(&self, index: usize) -> Option<&Frame> {
        self.frames.get(index)
    }

    /// The nearest enclosing call to a named function, looking out through lists and dicts.
    pub fn call_at(&self, offset: usize) -> Option<usize> {
        let mut index = self.frame_at(offset);
        while let Some(current) = index {
            let frame = self.frames.get(current)?;
            if frame.callee.is_some() {
                return Some(current);
            }
            index = frame.parent;
        }
        None
    }

    /// The slot of `frame` that holds `offset`.
    pub fn slot_at(&self, frame: usize, offset: usize) -> Option<&Slot> {
        self.frames
            .get(frame)?
            .slots
            .iter()
            .find(|slot| slot.span.start <= offset && offset <= slot.span.end)
    }

    /// The slot of `frame` that the nested position `offset` belongs to.
    pub fn slot_index(&self, frame: usize, offset: usize) -> Option<usize> {
        self.frames
            .get(frame)?
            .slots
            .iter()
            .position(|slot| slot.span.start <= offset && offset <= slot.span.end)
    }
}

#[cfg(test)]
mod tests {
    use crate::scan::*;

    fn at(source: &str) -> (String, usize) {
        let offset = source.find('|').expect("cursor marker");
        (source.replacen('|', "", 1), offset)
    }

    #[test]
    fn a_call_records_its_callee_and_keyword_slots() {
        let (source, cursor) = at("x = agent(name = \"a\", prompt = |)");
        let structure = scan(&source);
        let call = structure.call_at(cursor).expect("in a call");
        let frame = structure.frame(call).expect("frame");
        assert_eq!(frame.callee.as_deref(), Some("agent"));
        let keys: Vec<_> = frame
            .slots
            .iter()
            .filter_map(|s| s.key.as_ref())
            .map(|k| k.name.as_str())
            .collect();
        assert_eq!(keys, ["name", "prompt"]);
        let slot = structure.slot_at(call, cursor).expect("slot");
        assert_eq!(slot.key.as_ref().map(|k| k.name.as_str()), Some("prompt"));
    }

    #[test]
    fn an_unclosed_call_on_the_line_being_typed_still_has_structure() {
        let (source, cursor) = at("a = command(name = \"x\", ru|\nb = 1\n");
        let structure = scan(&source);
        let call = structure.call_at(cursor).expect("in a call");
        let frame = structure.frame(call).expect("frame");
        assert_eq!(frame.callee.as_deref(), Some("command"));
        assert_eq!(frame.end, None);
        let slot = structure.slot_at(call, cursor).expect("slot");
        assert_eq!(slot.key, None, "no `=` typed yet");
    }

    #[test]
    fn comparisons_and_compound_assignments_are_not_keywords() {
        let source = "f(a == b, c <= d, e != g)";
        let structure = scan(source);
        let frame = structure.frame(0).expect("frame");
        assert_eq!(frame.slots.len(), 3);
        assert!(frame.slots.iter().all(|slot| slot.key.is_none()));
    }

    #[test]
    fn lists_inside_a_call_resolve_to_the_call() {
        let (source, cursor) = at("agent(depends_on = [a, |])");
        let structure = scan(&source);
        let call = structure.call_at(cursor).expect("call");
        assert_ne!(
            Some(call),
            structure.frame_at(cursor),
            "the cursor is in the list"
        );
        let slot = structure.slot_at(call, cursor).expect("slot");
        assert_eq!(
            slot.key.as_ref().map(|k| k.name.as_str()),
            Some("depends_on")
        );
    }

    #[test]
    fn strings_comments_and_prefixes_hide_brackets() {
        let source = "x = f(\"(\", r'[', b\"\"\"{\n\"\"\") # g(\ny = 2";
        let structure = scan(source);
        assert_eq!(structure.frames.len(), 1, "{:?}", structure.frames);
        assert_eq!(structure.strings.len(), 3);
        assert!(structure.strings.iter().all(|s| s.closed));
        assert_eq!(structure.comments.len(), 1);
        let comment = structure.comments.first().expect("comment").clone();
        assert!(structure.in_comment(comment.end));
        assert!(!structure.in_comment(comment.start));
    }

    #[test]
    fn the_cursor_inside_an_unterminated_string_is_in_that_string() {
        let (source, cursor) = at("p = prompt_file(\"prompts/re|\nq = 1");
        let structure = scan(&source);
        let literal = structure.string_at(cursor).expect("in string");
        assert!(!literal.closed);
        assert_eq!(&source[literal.content.start..cursor], "prompts/re");
        let call = structure.call_at(cursor).expect("call");
        assert_eq!(
            structure.frame(call).and_then(|f| f.callee.as_deref()),
            Some("prompt_file")
        );
    }

    #[test]
    fn dict_entries_record_string_keys_and_the_assignment_target() {
        let (source, cursor) = at("params = {\n  \"repo\": {\"type\": \"|\"},\n}");
        let structure = scan(&source);
        let inner = structure.frame_at(cursor).expect("inner dict");
        let outer = structure
            .frame(inner)
            .and_then(|f| f.parent)
            .expect("outer");
        assert_eq!(
            structure.frame(outer).and_then(|f| f.target.as_deref()),
            Some("params")
        );
        let entry = structure
            .slot_at(outer, structure.frame(inner).expect("inner").start)
            .expect("entry");
        assert_eq!(entry.key.as_ref().map(|k| k.name.as_str()), Some("repo"));
        let field = structure.slot_at(inner, cursor).expect("field");
        assert_eq!(field.key.as_ref().map(|k| k.name.as_str()), Some("type"));
    }

    #[test]
    fn method_calls_and_keywords_are_not_callees() {
        let structure = scan("x = \"a\".format(1)\nif (y):\n  pass\n");
        assert!(
            structure.frames.iter().all(|f| f.callee.is_none()),
            "{:?}",
            structure.frames
        );
    }

    #[test]
    fn a_stray_closer_does_not_unwind_unrelated_frames() {
        let (source, cursor) = at("agent(name = \"a\" ] , |");
        let structure = scan(&source);
        let call = structure.call_at(cursor).expect("still in the call");
        assert_eq!(
            structure.frame(call).and_then(|f| f.callee.as_deref()),
            Some("agent")
        );
    }
}

//! A document as a request sees it.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use crucible::plan::starlark::params::ParamSpec;
use crucible::plan::workflow::WorkflowType;

use crate::index::{Index, ParamDecl};
use crate::pack::Pack;
use crate::scan::{Literal, Structure, scan};
use crate::text::Text;

/// What the pack's entry and manifest declare, shared by every document in the pack.
#[derive(Debug, Default)]
pub struct PackContext {
    pub pack: Option<Pack>,
    pub lane: Option<WorkflowType>,
    pub entry: Option<PathBuf>,
    pub params: Vec<ParamSpec>,
    pub param_decls: Vec<ParamDecl>,
    pub sandboxes: Option<BTreeSet<String>>,
}

pub struct Snapshot {
    pub path: Option<PathBuf>,
    pub text: Arc<Text>,
    pub structure: Structure,
    /// From the last version of the document that parsed.
    pub index: Option<Arc<Index>>,
    pub context: Arc<PackContext>,
}

/// Where a string literal sits in the DSL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StringSlot {
    Positional {
        callee: String,
        index: usize,
    },
    Kwarg {
        callee: String,
        name: String,
    },
    /// Inside one parameter's declaration in the `params` block.
    ParamField {
        key: bool,
        field: Option<String>,
    },
    /// A value of an `emits = {...}` dict.
    EmitsType,
    Other,
}

impl Snapshot {
    pub fn new(
        path: Option<PathBuf>,
        text: Arc<Text>,
        index: Option<Arc<Index>>,
        context: Arc<PackContext>,
    ) -> Self {
        let structure = scan(text.as_str());
        Snapshot {
            path,
            text,
            structure,
            index,
            context,
        }
    }

    pub fn source(&self) -> &str {
        self.text.as_str()
    }

    /// The identifier that ends at `offset`, possibly empty.
    pub fn word_before(&self, offset: usize) -> Range<usize> {
        let bytes = self.source().as_bytes();
        let mut start = offset.min(bytes.len());
        while start > 0 && bytes.get(start - 1).is_some_and(|&b| is_ident(b)) {
            start -= 1;
        }
        start..offset.min(bytes.len())
    }

    /// The identifier around `offset`, possibly empty.
    pub fn word_at(&self, offset: usize) -> Range<usize> {
        let bytes = self.source().as_bytes();
        let start = self.word_before(offset).start;
        let mut end = offset.min(bytes.len());
        while bytes.get(end).is_some_and(|&b| is_ident(b)) {
            end += 1;
        }
        start..end
    }

    /// The identifier before a `.` that directly precedes `start`.
    pub fn receiver(&self, start: usize) -> Option<String> {
        if start == 0 || self.source().as_bytes().get(start - 1) != Some(&b'.') {
            return None;
        }
        let object = self.word_before(start - 1);
        (!object.is_empty()).then(|| self.text.slice(object).to_owned())
    }

    pub fn string_slot(&self, literal: &Literal) -> StringSlot {
        let Some(frame_index) = literal.frame else {
            return StringSlot::Other;
        };
        let Some(frame) = self.structure.frame(frame_index) else {
            return StringSlot::Other;
        };
        match frame.open {
            b'(' => {
                let Some(callee) = frame.callee.clone() else {
                    return StringSlot::Other;
                };
                let Some(index) = self.structure.slot_index(frame_index, literal.span.start) else {
                    return StringSlot::Other;
                };
                match frame.slots.get(index).and_then(|slot| slot.key.as_ref()) {
                    Some(key) => StringSlot::Kwarg {
                        callee,
                        name: key.name.clone(),
                    },
                    None => StringSlot::Positional { callee, index },
                }
            }
            b'{' => {
                let key = self
                    .structure
                    .slot_at(frame_index, literal.span.start)
                    .and_then(|slot| slot.key.as_ref());
                let is_key = key.is_none_or(|key| key.span.start == literal.span.start);
                let field = key.filter(|_| !is_key).map(|key| key.name.clone());
                let Some(parent) = frame
                    .parent
                    .and_then(|p| Some((p, self.structure.frame(p)?)))
                else {
                    return StringSlot::Other;
                };
                if parent.1.open == b'{' && parent.1.target.as_deref() == Some("params") {
                    return StringSlot::ParamField { key: is_key, field };
                }
                let in_emits = parent.1.open == b'('
                    && self
                        .structure
                        .slot_at(parent.0, frame.start)
                        .and_then(|slot| slot.key.as_ref())
                        .is_some_and(|key| key.name == "emits");
                if in_emits && !is_key {
                    StringSlot::EmitsType
                } else {
                    StringSlot::Other
                }
            }
            _ => StringSlot::Other,
        }
    }

    /// The content of the string literal written as the `index`th argument of the call in
    /// `frame`.
    pub fn argument_string(&self, frame: usize, index: usize) -> Option<&str> {
        let slot = self.structure.frame(frame)?.slots.get(index)?.span.clone();
        let literal = self
            .structure
            .strings
            .iter()
            .find(|s| s.frame == Some(frame) && slot.contains(&s.span.start))?;
        Some(self.text.slice(literal.content.clone()))
    }
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use crate::snapshot::*;

    fn at(source: &str) -> (Snapshot, usize) {
        let offset = source.find('|').expect("cursor");
        let text = Arc::new(Text::new(source.replacen('|', "", 1)));
        (Snapshot::new(None, text, None, Arc::default()), offset)
    }

    fn slot(source: &str) -> StringSlot {
        let (snapshot, offset) = at(source);
        let literal = snapshot
            .structure
            .string_at(offset)
            .expect("in a string")
            .clone();
        snapshot.string_slot(&literal)
    }

    #[test]
    fn strings_know_which_argument_they_are() {
        assert_eq!(
            slot("prompt_file(\"pro|\")"),
            StringSlot::Positional {
                callee: "prompt_file".into(),
                index: 0
            }
        );
        assert_eq!(
            slot("agent(name = \"a\", join = \"pa|\")"),
            StringSlot::Kwarg {
                callee: "agent".into(),
                name: "join".into()
            }
        );
        assert_eq!(
            slot("params = {\"repo\": {\"ty|\"}}"),
            StringSlot::ParamField {
                key: true,
                field: None
            }
        );
        assert_eq!(
            slot("params = {\"repo\": {\"type\": \"st|\"}}"),
            StringSlot::ParamField {
                key: false,
                field: Some("type".into())
            }
        );
        assert_eq!(
            slot("agent(emits = {\"verdict\": \"str|\"})"),
            StringSlot::EmitsType
        );
        assert_eq!(slot("agent(emits = {\"ver|\"})"), StringSlot::Other);
        assert_eq!(slot("x = \"a|\""), StringSlot::Other);
    }

    #[test]
    fn words_and_receivers_are_read_around_the_cursor() {
        let (snapshot, offset) = at("over = review.ver|dict");
        assert_eq!(snapshot.text.slice(snapshot.word_before(offset)), "ver");
        assert_eq!(snapshot.text.slice(snapshot.word_at(offset)), "verdict");
        assert_eq!(
            snapshot.receiver(snapshot.word_at(offset).start).as_deref(),
            Some("review")
        );
        assert_eq!(snapshot.receiver(0), None);
    }

    #[test]
    fn an_argument_string_is_read_by_position() {
        let (snapshot, offset) = at("load(\"lib.star\", \"|\")");
        let call = snapshot.structure.call_at(offset).expect("call");
        assert_eq!(snapshot.argument_string(call, 0), Some("lib.star"));
    }
}

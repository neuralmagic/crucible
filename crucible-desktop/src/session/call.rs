//! A call task's target, and the target runs its session says it dispatched.

use std::collections::BTreeMap;

use crucible_contract::calls::{CallAlias, CallId, CallKind, NodeLink, TargetHandle};
use crucible_contract::session::PlanCallWire;

#[derive(Clone, Debug, PartialEq)]
pub struct CallTask {
    pub alias: CallAlias,
    pub kind: CallKind,
    pub params: BTreeMap<String, String>,
    pub dispatched: Vec<Dispatched>,
}

/// One target run, as the `call_dispatched` event recorded it.
#[derive(Clone, Debug, PartialEq)]
pub struct Dispatched {
    pub instance: Option<String>,
    pub attempt: u32,
    pub call: CallId,
    pub handle: TargetHandle,
    pub link: Option<NodeLink>,
}

impl CallTask {
    pub fn from_wire(wire: PlanCallWire) -> Self {
        CallTask {
            alias: wire.alias,
            kind: wire.target,
            params: wire.params,
            dispatched: Vec::new(),
        }
    }

    pub fn dispatch(&self, instance: Option<&str>) -> Option<&Dispatched> {
        self.dispatched
            .iter()
            .find(|d| d.instance.as_deref() == instance)
    }

    pub fn link(&self) -> Option<&NodeLink> {
        self.dispatch(None)?.link.as_ref()
    }

    pub fn record(&mut self, dispatched: Dispatched) {
        match self
            .dispatched
            .iter_mut()
            .find(|d| d.instance == dispatched.instance)
        {
            Some(held) if held.attempt <= dispatched.attempt => *held = dispatched,
            Some(_) => {}
            None => self.dispatched.push(dispatched),
        }
    }

    pub fn bound_by<'a>(&'a self, producer: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.params
            .iter()
            .filter(move |(_, by)| *by == producer)
            .map(|(param, _)| param.as_str())
    }
}

pub fn badge(kind: CallKind) -> &'static str {
    kind.presentation().badge
}

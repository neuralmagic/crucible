//! A call task's target, and the target runs its session says it dispatched.

use std::collections::BTreeMap;

use crucible_contract::calls::{CallAlias, CallId, CallKind, NodeLink, TargetHandle};
use crucible_contract::session::PlanCallWire;

#[derive(Clone, Debug, PartialEq)]
pub struct CallTask {
    pub alias: CallAlias,
    pub kind: CallKind,
    /// Each parameter a dependency binds, by name, and the task that produces it.
    pub params: BTreeMap<String, String>,
    /// The newest dispatch of the task, or of each of its instances.
    pub dispatched: Vec<Dispatched>,
}

/// One target run, as the `call_dispatched` event recorded it.
#[derive(Clone, Debug, PartialEq)]
pub struct Dispatched {
    /// The instance's item key; `None` for an unmapped call.
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

    /// The target run of the task itself, or of instance `instance`.
    pub fn dispatch(&self, instance: Option<&str>) -> Option<&Dispatched> {
        self.dispatched
            .iter()
            .find(|d| d.instance.as_deref() == instance)
    }

    /// Where the task's own target run opens.
    pub fn link(&self) -> Option<&NodeLink> {
        self.dispatch(None)?.link.as_ref()
    }

    /// Records `dispatched`, replacing an older attempt of the same instance.
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

    /// The parameters `producer` binds, by name.
    pub fn bound_by<'a>(&'a self, producer: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.params
            .iter()
            .filter(move |(_, by)| *by == producer)
            .map(|(param, _)| param.as_str())
    }
}

/// The short name a graph badge gives a target kind.
pub fn badge(kind: CallKind) -> &'static str {
    kind.presentation().badge
}

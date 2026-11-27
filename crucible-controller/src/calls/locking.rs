//! Writing a pack's `calls.lock`: each declared call target resolved and pinned.

use std::collections::{BTreeMap, BTreeSet};

use crucible_contract::calls::{
    CallAlias, CallTarget, CallsLock, LOCK_FILE, LOCK_VERSION, LockedCall, Pin, PlaybookRef,
};

use crate::calls::lock::{declared, of_tree};
use crate::calls::{Backends, CallCtx, CallError};

/// Who a lock is written for, and of what.
pub(crate) struct Locking<'a> {
    pub state: &'a crate::api::state::ApiState,
    pub caller: &'a crate::authz::Caller,
    pub draft_pack: bool,
    pub draft: Option<&'a str>,
}

fn no_target(alias: &CallAlias, playbook: &PlaybookRef) -> String {
    match playbook {
        PlaybookRef::Registered(name) => {
            format!("call target {alias}: no registered playbook {name:?}")
        }
        PlaybookRef::Draft(id) => format!("call target {alias}: no draft {id:?}"),
    }
}

pub(crate) async fn write(
    backends: &Backends,
    ctx: &CallCtx,
    calls: &BTreeMap<CallAlias, CallTarget>,
    locking: &Locking<'_>,
) -> Result<Option<CallsLock>, String> {
    if calls.is_empty() {
        return Ok(None);
    }
    let mut locked = BTreeMap::new();
    for (alias, target) in calls {
        if let CallTarget::Playbook { playbook } = target {
            let readable =
                crate::calls::access::may_read_target(locking.state, locking.caller, playbook)
                    .await
                    .map_err(|e| format!("call target {alias}: {e:#}"))?;
            if !readable {
                return Err(no_target(alias, playbook));
            }
        }
        let failed = |e: CallError| match e {
            CallError::Internal(e) => format!("call target {alias}: {e:#}"),
            other => format!("call target {alias}: {}", other.note()),
        };
        let backend = backends.get(target.kind()).map_err(failed)?;
        let who = locking
            .caller
            .actor()
            .cloned()
            .unwrap_or_else(crate::authz::model::Principal::platform);
        backend.authorize(ctx, &who, target).await.map_err(failed)?;
        let resolved = backend.interface(ctx, target).await.map_err(failed)?;
        if let Pin::DraftHead { draft } = &resolved.pin {
            if !locking.draft_pack {
                return Err(format!(
                    "call target {alias} pins a draft's head, which only a draft pack may do; \
                     register the target and name it by its registered name"
                ));
            }
            if let Some(cycle) = draft_cycle(ctx, draft, locking.draft)
                .await
                .map_err(|e| format!("call target {alias}: {e:#}"))?
            {
                return Err(format!(
                    "call target {alias} leads back to this draft through draft heads: {}",
                    cycle.join(" -> ")
                ));
            }
        }
        let entry = LockedCall::new(resolved.pin, resolved.interface)
            .map_err(|e| format!("call target {alias}: digesting the interface: {e}"))?;
        locked.insert(alias.clone(), entry);
    }
    Ok(Some(CallsLock {
        version: LOCK_VERSION,
        calls: locked,
    }))
}

async fn draft_cycle(
    ctx: &CallCtx,
    start: &str,
    locking: Option<&str>,
) -> anyhow::Result<Option<Vec<String>>> {
    let mut path: Vec<String> = locking.map(str::to_string).into_iter().collect();
    let mut stack: Vec<(String, usize)> = vec![(start.to_string(), path.len())];
    let mut seen: BTreeSet<String> = BTreeSet::new();
    while let Some((draft, depth)) = stack.pop() {
        path.truncate(depth);
        if path.contains(&draft) {
            path.push(draft);
            return Ok(Some(path));
        }
        path.push(draft.clone());
        if !seen.insert(draft.clone()) {
            continue;
        }
        let tree = match crate::playbooks::drafts::newest_compiling(ctx.db.pool(), &draft).await {
            Ok((_, tree)) => tree,
            Err(crate::playbooks::drafts::DraftError::Internal(e)) => return Err(e),
            Err(_) => continue,
        };
        let Ok((_, Some(lock))) = of_tree(&tree) else {
            continue;
        };
        for entry in lock.calls.values() {
            if let Pin::DraftHead { draft: next } = &entry.pin {
                stack.push((next.clone(), path.len()));
            }
        }
    }
    Ok(None)
}

pub(crate) async fn into_files(
    backends: &Backends,
    ctx: &CallCtx,
    locking: &Locking<'_>,
    files: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let calls = declared(|path| files.get(path).cloned())?;
    match write(backends, ctx, &calls, locking).await? {
        Some(lock) => {
            let text = lock
                .render()
                .map_err(|e| format!("rendering {LOCK_FILE}: {e}"))?;
            files.insert(LOCK_FILE.to_string(), text);
        }
        None => {
            files.remove(LOCK_FILE);
        }
    }
    Ok(())
}

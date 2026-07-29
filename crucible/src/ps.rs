//! `crucible ps`: every crucible loop pod running in the cluster, one row each.
//!
//! Fetching is a thin call into [`forge::kube::list_pods`] against the selector every rendered loop pod
//! carries (`app.kubernetes.io/managed-by=crucible`, [`crate::deploy::MANAGED_BY_LABEL`]). Everything
//! past the fetch (building a row from a `Pod`, formatting the table, serializing JSON) is pure and
//! covered by tests off constructed `Pod` objects; no live cluster, no mock of the kube transport.
//!
//! ITER (current iteration / total) is a design note, not shipped: the loop's progress lives behind the
//! in-pod TCP control bridge (`crucible/src/control.rs`, port 7777), a raw NDJSON protocol with no HTTP
//! front end. Reaching it from here needs either a direct pod-IP dial (namespace network policy
//! dependent, no existing precedent in this codebase) or an in-pod exec shimming the protocol over a
//! shell TCP trick, both fragile and, per the constraint that a live cluster and kube-API mocks are both
//! off the table for tests, not something this change can cover with the rigor the rest of `ps` gets. The
//! column ships now (prints `-`) so the table shape doesn't change again once enrichment lands.

use anyhow::Result;
use k8s_openapi::api::core::v1::Pod;
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PodRow {
    pub name: String,
    pub namespace: String,
    pub phase: String,
    pub age: String,
    pub restarts: i32,
    /// Current/total agent iteration, best-effort. `None` today (see the module doc), reserved so
    /// `--json` consumers don't need a schema change once enrichment lands.
    pub iter: Option<String>,
}

/// List every crucible loop pod (`ns = None` = every namespace the client can see) and print either the
/// aligned table or, with `json`, the same rows as a JSON array.
pub fn run(namespace: Option<&str>, json: bool) -> Result<()> {
    let pods = forge::kube::list_pods(namespace, crate::deploy::MANAGED_BY_LABEL)?;
    let now = now_epoch_secs();
    let rows: Vec<PodRow> = pods.iter().map(|p| pod_to_row(p, now)).collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        print!("{}", format_table(&rows));
    }
    Ok(())
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Project one `Pod` into a display row. `now_epoch` is threaded in (rather than read live) so this stays
/// pure and testable against a fixed clock.
fn pod_to_row(pod: &Pod, now_epoch: i64) -> PodRow {
    let name = pod.metadata.name.clone().unwrap_or_default();
    let namespace = pod.metadata.namespace.clone().unwrap_or_default();
    let phase = pod
        .status
        .as_ref()
        .and_then(|s| s.phase.clone())
        .unwrap_or_else(|| "Unknown".to_string());
    let age = pod
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|t| format_age(now_epoch - t.0.as_second()))
        .unwrap_or_else(|| "-".to_string());
    let restarts = pod
        .status
        .as_ref()
        .and_then(|s| s.container_statuses.as_ref())
        .map(|cs| cs.iter().map(|c| c.restart_count).sum())
        .unwrap_or(0);
    PodRow {
        name,
        namespace,
        phase,
        age,
        restarts,
        iter: None,
    }
}

/// kubectl-style single-unit age: `45s`, `12m`, `3h`, `5d`. Negative/zero (clock skew, brand-new pod)
/// floors at `0s`.
fn format_age(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Plain aligned text: NAME, NAMESPACE, PHASE, AGE, RESTARTS, ITER, no TUI, so it pipes through `grep`/
/// `awk` like any other `ps`. `-` for a missing ITER (always, today, see the module doc).
fn format_table(rows: &[PodRow]) -> String {
    let headers = ["NAME", "NAMESPACE", "PHASE", "AGE", "RESTARTS", "ITER"];
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|r| {
            [
                r.name.clone(),
                r.namespace.clone(),
                r.phase.clone(),
                r.age.clone(),
                r.restarts.to_string(),
                r.iter.clone().unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();

    let mut widths = headers.map(str::len);
    for row in &cells {
        for (w, cell) in widths.iter_mut().zip(row.iter()) {
            *w = (*w).max(cell.len());
        }
    }

    let mut out = String::new();
    let render_row = |cols: &[String; 6], widths: &[usize; 6], out: &mut String| {
        for (i, (col, w)) in cols.iter().zip(widths.iter()).enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            if i + 1 == cols.len() {
                out.push_str(col); // last column: no trailing pad
            } else {
                out.push_str(&format!("{col:<w$}"));
            }
        }
        out.push('\n');
    };
    render_row(&headers.map(str::to_string), &widths, &mut out);
    for row in &cells {
        render_row(row, &widths, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(name: &str, ns: &str, phase: &str, created_epoch: i64, restarts: i32) -> Pod {
        serde_json::from_value(serde_json::json!({
            "metadata": {
                "name": name,
                "namespace": ns,
                "creationTimestamp": chrono_stamp(created_epoch),
            },
            "status": {
                "phase": phase,
                "containerStatuses": [
                    { "name": "loop", "ready": true, "restartCount": restarts,
                      "image": "x", "imageID": "", "state": {} }
                ]
            }
        }))
        .expect("constructed pod parses")
    }

    /// RFC3339 stamp for a fixed epoch-seconds value (k8s `Time` round-trips through this format).
    fn chrono_stamp(epoch: i64) -> String {
        k8s_openapi::jiff::Timestamp::from_second(epoch)
            .expect("valid timestamp")
            .strftime("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    #[test]
    fn format_age_picks_the_largest_sane_unit() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(45), "45s");
        assert_eq!(format_age(59), "59s");
        assert_eq!(format_age(60), "1m");
        assert_eq!(format_age(3599), "59m");
        assert_eq!(format_age(3600), "1h");
        assert_eq!(format_age(86399), "23h");
        assert_eq!(format_age(86400), "1d");
        assert_eq!(format_age(-5), "0s", "clock skew floors at 0");
    }

    #[test]
    fn pod_to_row_projects_name_namespace_phase_age_restarts() {
        let now = 10_000;
        let p = pod("epp-loop", "autoresearch", "Running", now - 125, 2);
        let row = pod_to_row(&p, now);
        assert_eq!(row.name, "epp-loop");
        assert_eq!(row.namespace, "autoresearch");
        assert_eq!(row.phase, "Running");
        assert_eq!(row.age, "2m");
        assert_eq!(row.restarts, 2);
        assert_eq!(
            row.iter, None,
            "ITER enrichment not shipped (see module doc)"
        );
    }

    #[test]
    fn pod_to_row_defaults_missing_phase_to_unknown() {
        let p: Pod = serde_json::from_value(serde_json::json!({
            "metadata": { "name": "x", "namespace": "ns" }
        }))
        .unwrap();
        let row = pod_to_row(&p, 0);
        assert_eq!(row.phase, "Unknown");
        assert_eq!(row.age, "-", "no creationTimestamp");
        assert_eq!(row.restarts, 0);
    }

    #[test]
    fn format_table_aligns_columns_and_headers() {
        let rows = vec![
            PodRow {
                name: "epp-loop".into(),
                namespace: "autoresearch".into(),
                phase: "Running".into(),
                age: "2m".into(),
                restarts: 0,
                iter: None,
            },
            PodRow {
                name: "pd-rollout-loop".into(),
                namespace: "rig".into(),
                phase: "Succeeded".into(),
                age: "1d".into(),
                restarts: 3,
                iter: None,
            },
        ];
        let table = format_table(&rows);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3, "header + 2 rows");
        assert!(lines[0].starts_with("NAME"));
        assert!(lines[0].contains("NAMESPACE"));
        assert!(lines[0].contains("RESTARTS"));
        assert!(lines[0].contains("ITER"));
        assert!(lines[1].contains("epp-loop"));
        assert!(lines[1].trim_end().ends_with('-'), "missing ITER prints -");
        assert!(lines[2].contains("pd-rollout-loop"));
        // Column starts line up: the NAMESPACE column starts at the same byte offset on every row.
        let ns_col = lines[0].find("NAMESPACE").unwrap();
        assert!(lines[1][ns_col..].starts_with("autoresearch"));
        assert!(lines[2][ns_col..].starts_with("rig"));
    }

    #[test]
    fn format_table_empty_prints_header_only() {
        let table = format_table(&[]);
        assert_eq!(table.lines().count(), 1);
        assert!(table.starts_with("NAME"));
    }

    #[test]
    fn rows_serialize_to_json_with_null_iter() {
        let rows = vec![PodRow {
            name: "epp-loop".into(),
            namespace: "ns".into(),
            phase: "Running".into(),
            age: "5m".into(),
            restarts: 1,
            iter: None,
        }];
        let json = serde_json::to_string(&rows).unwrap();
        assert!(json.contains("\"name\":\"epp-loop\""));
        assert!(json.contains("\"iter\":null"));
    }
}

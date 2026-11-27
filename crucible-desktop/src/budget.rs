//! The request budget of an idle app: the workers one profile runs, as [`Remote::new`] starts them
//! and as the window points them at an idle Runs page, counted on the wire.
//!
//! ```text
//!   Remote::new ── live, admin, finder workers ──▶ loopback controller ── counts each request
//!                                                   (or a counting proxy ──▶ a real controller)
//! ```
//!
//! The budgets agreed for one idle desktop on one profile, nothing running: at most 12 requests a
//! minute while its window is focused and 3 while it is not. The loopback controller answers the
//! routes an idle app reads and honours `If-None-Match`, so the counts include the conditional
//! reads. `idle_desktops_against_a_controller` runs the same workers, any number of them, against
//! a real controller:
//!
//! ```text
//!   CRUCIBLE_BUDGET_URL=http://127.0.0.1:8896 CRUCIBLE_BUDGET_DESKTOPS=20 \
//!     cargo test --release budget::tests::idle_desktops_against_a_controller -- --ignored --nocapture
//! ```

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::hash::{Hash, Hasher};
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::admin::Watch;
    use crate::httpcache::Store;
    use crate::net::Net;
    use crate::remote::Remote;
    use crate::testserver::{Reply, serve_replies, target};

    const WARM: Duration = Duration::from_secs(5);
    const SETTLE: Duration = Duration::from_secs(12);
    const MINUTE: Duration = Duration::from_secs(60);
    const FOCUSED: usize = 12;
    const UNFOCUSED: usize = 3;

    /// Each request seen: when, its route, whether it carried `If-None-Match`, and its status.
    #[derive(Clone, Debug)]
    struct Seen {
        at: Instant,
        route: String,
        conditional: bool,
        status: u16,
    }

    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<Seen>>>);

    impl Log {
        fn push(&self, seen: Seen) {
            self.0.lock().unwrap().push(seen);
        }

        fn between(&self, from: Instant, to: Instant) -> Vec<Seen> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|seen| seen.at >= from && seen.at < to)
                .cloned()
                .collect()
        }
    }

    /// One app's workers on one profile, with a response cache of its own.
    struct Desktop {
        remote: Remote,
        _net: Net,
        _dir: tempfile::TempDir,
    }

    impl Desktop {
        fn open(url: &str) -> Desktop {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(&dir.path().join("cache.sqlite")).unwrap();
            let net = Net::cached_for_tests(Arc::new(store));
            let target = target("budget", url, dir.path());
            let remote = Remote::new(target, &net, None, None);
            remote.admin_client.watch(Watch {
                controller: true,
                teams: true,
                ..Watch::default()
            });
            Desktop {
                remote,
                _net: net,
                _dir: dir,
            }
        }

        fn focus(&self, focused: bool) {
            self.remote.live_client.focus(focused);
            self.remote.admin_client.focus(focused);
        }
    }

    fn route(line: &str) -> String {
        let path = line.split_whitespace().nth(1).unwrap_or_default();
        let path = path.split('?').next().unwrap_or_default();
        format!(
            "{} {path}",
            line.split_whitespace().next().unwrap_or_default()
        )
    }

    fn tag(body: &str) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        body.hash(&mut hasher);
        format!("W/\"{:016x}\"", hasher.finish())
    }

    fn idle_controller(log: Log) -> String {
        let (url, _) = serve_replies(move |asked| {
            let body = match route(&asked.line).as_str() {
                "GET /api/whoami" => r#"{"user":"wren","role":"user"}"#.to_string(),
                "GET /api/playbook-runs" => serde_json::json!([
                    {"key": "playbook:demo:01", "playbook": "demo", "status": "valid",
                     "cost_usd": 0.4, "max_cost": 1.0, "runs": 1,
                     "created_at": "2026-11-26T10:00:00Z", "created_by": "wren"},
                    {"key": "playbook:demo:02", "playbook": "demo", "status": "error",
                     "cost_usd": 0.1, "max_cost": 1.0, "runs": 1,
                     "created_at": "2026-11-26T11:00:00Z", "created_by": "wren"},
                ])
                .to_string(),
                "GET /api/playbook-drafts" => serde_json::json!([
                    {"id": "studio", "description": "a draft", "owner": "user:wren",
                     "latest_version": 2, "compiles": true,
                     "updated_at": "2026-11-26T09:00:00Z"},
                ])
                .to_string(),
                "GET /api/playbooks" => serde_json::json!([
                    {"id": "demo", "description": "a pack", "source": {"kind": "git",
                     "repo": "o/r", "path": "demo"}, "rev": "abc", "created_by": "wren"},
                ])
                .to_string(),
                "GET /api/decisions" => r#"{"open":[]}"#.to_string(),
                "GET /api/approvals" => "{}".to_string(),
                _ => "[]".to_string(),
            };
            let etag = tag(&body);
            let conditional = asked.header("if-none-match").is_some();
            let unchanged = asked.header("if-none-match") == Some(etag.as_str());
            log.push(Seen {
                at: Instant::now(),
                route: route(&asked.line),
                conditional,
                status: if unchanged { 304 } else { 200 },
            });
            if unchanged {
                Reply {
                    status: 304,
                    headers: vec![("etag", etag)],
                    body: String::new(),
                }
            } else {
                Reply {
                    status: 200,
                    headers: vec![("etag", etag)],
                    body,
                }
            }
        });
        url
    }

    fn per_route(seen: &[Seen]) -> BTreeMap<String, usize> {
        let mut routes = BTreeMap::new();
        for seen in seen {
            *routes.entry(seen.route.clone()).or_default() += 1;
        }
        routes
    }

    #[test]
    fn an_idle_focused_desktop_stays_within_its_request_budget() {
        let log = Log::default();
        let url = idle_controller(log.clone());
        let _desktop = Desktop::open(&url);
        std::thread::sleep(WARM);
        let from = Instant::now();
        std::thread::sleep(MINUTE);
        let seen = log.between(from, Instant::now());
        assert!(
            seen.len() <= FOCUSED,
            "{} requests in a minute, budget {FOCUSED}: {:?}",
            seen.len(),
            per_route(&seen)
        );
        let runs: Vec<&Seen> = seen
            .iter()
            .filter(|s| s.route == "GET /api/playbook-runs")
            .collect();
        assert!(runs.len() >= 5, "the launch list went stale: {runs:?}");
        assert!(
            runs.iter().all(|s| s.conditional && s.status == 304),
            "an unchanged launch list was read in full: {runs:?}"
        );
    }

    #[test]
    fn an_idle_unfocused_desktop_stays_within_its_request_budget() {
        let log = Log::default();
        let url = idle_controller(log.clone());
        let desktop = Desktop::open(&url);
        std::thread::sleep(WARM);
        desktop.focus(false);
        std::thread::sleep(SETTLE);
        let from = Instant::now();
        std::thread::sleep(MINUTE);
        let seen = log.between(from, Instant::now());
        assert!(
            seen.len() <= UNFOCUSED,
            "{} requests in a minute, budget {UNFOCUSED}: {:?}",
            seen.len(),
            per_route(&seen)
        );
        assert!(
            seen.iter().any(|s| s.route == "GET /api/playbook-runs"),
            "an unfocused window stopped reading the launch list: {seen:?}"
        );

        let back = Instant::now();
        desktop.focus(true);
        let woken = ["GET /api/playbook-runs", "GET /api/playbook-drafts"];
        let mut woke = BTreeMap::new();
        while back.elapsed() < Duration::from_secs(5) {
            woke = per_route(&log.between(back, Instant::now()));
            if woken.iter().all(|route| woke.contains_key(*route)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        for route in woken {
            assert!(
                woke.contains_key(route),
                "{route} was not read on focus: {woke:?}"
            );
        }
    }

    fn counting_proxy(upstream: &str, log: Log) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let upstream = upstream
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                let (upstream, log) = (upstream.clone(), log.clone());
                std::thread::spawn(move || relay(client, &upstream, log));
            }
        });
        url
    }

    fn relay(client: TcpStream, upstream: &str, log: Log) {
        let Ok(server) = TcpStream::connect(upstream) else {
            return;
        };
        let mut from_client = BufReader::new(client.try_clone().unwrap());
        let mut to_client = client;
        let mut from_server = BufReader::new(server.try_clone().unwrap());
        let mut to_server = server;
        loop {
            let Some((head, body)) = message(&mut from_client) else {
                return;
            };
            let line = head.lines().next().unwrap_or_default().to_string();
            let conditional = head.to_ascii_lowercase().contains("\r\nif-none-match:");
            if to_server.write_all(head.as_bytes()).is_err() || to_server.write_all(&body).is_err()
            {
                return;
            }
            let Some((answer, answer_body)) = message(&mut from_server) else {
                return;
            };
            let status = answer
                .split_whitespace()
                .nth(1)
                .and_then(|code| code.parse().ok())
                .unwrap_or(0);
            log.push(Seen {
                at: Instant::now(),
                route: route(&line),
                conditional,
                status,
            });
            if to_client.write_all(answer.as_bytes()).is_err()
                || to_client.write_all(&answer_body).is_err()
            {
                return;
            }
        }
    }

    fn message(reader: &mut impl BufRead) -> Option<(String, Vec<u8>)> {
        let mut head = String::new();
        let mut length = 0;
        let mut chunked = false;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            if let Some((name, value)) = line.split_once(':') {
                let (name, value) = (name.trim(), value.trim());
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.parse().ok()?;
                }
                if name.eq_ignore_ascii_case("transfer-encoding") {
                    chunked = value.eq_ignore_ascii_case("chunked");
                }
            }
            head.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        if !chunked {
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).ok()?;
            return Some((head, body));
        }
        let mut body = Vec::new();
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).ok()?;
            body.extend_from_slice(size.as_bytes());
            let n = usize::from_str_radix(size.trim().split(';').next()?, 16).ok()?;
            let mut chunk = vec![0u8; n + 2];
            reader.read_exact(&mut chunk).ok()?;
            body.extend_from_slice(&chunk);
            if n == 0 {
                return Some((head, body));
            }
        }
    }

    fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    #[test]
    #[ignore = "drives a real controller: set CRUCIBLE_BUDGET_URL"]
    fn idle_desktops_against_a_controller() {
        let upstream = std::env::var("CRUCIBLE_BUDGET_URL").expect("CRUCIBLE_BUDGET_URL");
        let desktops: usize = env("CRUCIBLE_BUDGET_DESKTOPS", 1);
        let secs: u64 = env("CRUCIBLE_BUDGET_SECS", 120);
        let focused: bool = env("CRUCIBLE_BUDGET_FOCUSED", true);
        let log = Log::default();
        let url = counting_proxy(&upstream, log.clone());
        let stagger = Duration::from_secs(10) / desktops.max(1) as u32;
        let opened: Vec<Desktop> = (0..desktops)
            .map(|_| {
                std::thread::sleep(stagger);
                Desktop::open(&url)
            })
            .collect();
        for desktop in &opened {
            desktop.focus(focused);
        }
        std::thread::sleep(WARM + if focused { Duration::ZERO } else { SETTLE });
        let from = Instant::now();
        std::thread::sleep(Duration::from_secs(secs));
        let seen = log.between(from, Instant::now());
        let minutes = secs as f64 / 60.0;
        let per_desktop = |n: usize| n as f64 / minutes / desktops.max(1) as f64;
        println!(
            "{desktops} desktops, {} for {secs}s",
            if focused { "focused" } else { "unfocused" }
        );
        println!("{:<36} {:>10} {:>8}", "route", "req/min/d", "304");
        for (route, n) in per_route(&seen) {
            let unchanged = seen
                .iter()
                .filter(|s| s.route == route && s.status == 304)
                .count();
            println!("{route:<36} {:>10.2} {unchanged:>8}", per_desktop(n));
        }
        println!("{:<36} {:>10.2}", "total", per_desktop(seen.len()));
        let budget = if focused { FOCUSED } else { UNFOCUSED };
        assert!(
            per_desktop(seen.len()) <= budget as f64,
            "over the {budget} requests a minute an idle desktop may send"
        );
    }
}

//! The Dock and the menu bar: a badge counting what waits on the person, the budget a pinned
//! launch has used drawn on the Dock icon, and a menu bar item listing the launches running on the
//! profile in use.
//!
//! What they show is worked out from the app's state on every poll and handed to AppKit only when
//! it changes.

use rune_mac::status::Line;

use crate::App;
use crate::live::LivePlaybook;

/// What the Dock and the menu bar show.
#[derive(Clone, Debug, PartialEq)]
pub struct Shown {
    pub badge: Option<String>,
    /// The pinned launch's budget used, in thousandths, so a tiny change does not redraw.
    pub progress: Option<u16>,
    pub title: String,
    pub lines: Vec<Line>,
}

/// A running launch, as the menu lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct Running {
    pub key: String,
    pub playbook: String,
    pub cost: f64,
    pub cap: f64,
    pub elapsed: i64,
    pub max: Option<i64>,
}

impl Running {
    fn of(launch: &LivePlaybook, now: i64) -> Running {
        Running {
            key: launch.key.clone(),
            playbook: launch.id.clone(),
            cost: launch.cost.unwrap_or_default(),
            cap: launch.cap,
            elapsed: (now - launch.landed).max(0),
            max: crate::form::minutes(&launch.max_time).map(|m| m as i64 * 60),
        }
    }

    /// The share of its budget used: the larger of spend over its cost cap and time over its
    /// time cap, when it has either.
    pub fn budget(&self) -> Option<f64> {
        let spend = (self.cap > 0.0).then(|| self.cost / self.cap);
        let time = self
            .max
            .filter(|max| *max > 0)
            .map(|max| self.elapsed as f64 / max as f64);
        match (spend, time) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
        .map(|used| used.clamp(0.0, 1.0))
    }

    fn line(&self) -> String {
        let mut parts = vec![self.playbook.clone()];
        parts.push(if self.cap > 0.0 {
            format!("${:.2} of ${:.2}", self.cost, self.cap)
        } else {
            format!("${:.2}", self.cost)
        });
        parts.push(match self.max {
            Some(max) => format!("{} of {}", clock(self.elapsed), clock(max)),
            None => clock(self.elapsed),
        });
        if let Some(used) = self.budget() {
            parts.push(format!("{:.0}%", used * 100.0));
        }
        parts.join(" \u{b7} ")
    }
}

fn clock(secs: i64) -> String {
    let minutes = secs / 60;
    if minutes >= 60 {
        format!("{}h{:02}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

/// The launches running now, newest first.
pub fn running(app: &App) -> Vec<Running> {
    let crate::live::LiveView::Ready { now, launches, .. } = &app.live else {
        return Vec::new();
    };
    let mut running: Vec<&LivePlaybook> = launches
        .iter()
        .filter(|l| l.verdict() == crate::model::Verdict::Running)
        .collect();
    running.sort_by_key(|l| std::cmp::Reverse(l.landed));
    running.into_iter().map(|l| Running::of(l, *now)).collect()
}

/// What to show for `running` launches on `profile`, with `waiting` decisions and `pinned` the
/// launch whose budget the Dock shows.
pub fn shown(profile: &str, running: &[Running], waiting: usize, pinned: Option<&str>) -> Shown {
    let progress = pinned
        .and_then(|key| running.iter().find(|r| r.key == key))
        .and_then(Running::budget)
        .map(|used| (used * 1000.0).round() as u16);
    let title = match running.len() {
        0 => "\u{25c7}".to_string(),
        n => format!("\u{25c6} {n}"),
    };
    let mut lines = vec![Line::Label(if running.is_empty() {
        format!("Nothing running on {profile}")
    } else {
        format!("Running on {profile}")
    })];
    lines.extend(
        running
            .iter()
            .map(|r| Line::item(format!("open:{}", r.key), r.line())),
    );
    if !running.is_empty() {
        lines.push(Line::Separator);
        lines.push(Line::Label("Budget on Dock".into()));
        lines.push(Line::item("pin:", "None").checked(progress.is_none()));
        lines.extend(running.iter().map(|r| {
            Line::item(format!("pin:{}", r.key), r.playbook.clone())
                .checked(pinned == Some(r.key.as_str()))
        }));
    }
    lines.push(Line::Separator);
    if waiting > 0 {
        let noun = if waiting == 1 {
            "decision"
        } else {
            "decisions"
        };
        lines.push(Line::item("decisions", format!("{waiting} {noun} waiting")));
    }
    lines.push(Line::item("show", "Open Crucible Playbooks"));
    Shown {
        badge: (waiting > 0).then(|| waiting.to_string()),
        progress,
        title,
        lines,
    }
}

/// What a pick from the menu does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    Open(String),
    Pin(Option<String>),
    Decisions,
    Show,
}

pub fn pick(id: &str) -> Option<Pick> {
    if let Some(key) = id.strip_prefix("open:") {
        return Some(Pick::Open(key.to_string()));
    }
    if let Some(key) = id.strip_prefix("pin:") {
        return Some(Pick::Pin((!key.is_empty()).then(|| key.to_string())));
    }
    match id {
        "decisions" => Some(Pick::Decisions),
        "show" => Some(Pick::Show),
        _ => None,
    }
}

impl App {
    /// Hands the Dock and the menu bar what they should show now, when it changed.
    pub fn sync_tray(&mut self) {
        if rune::is_headless() {
            return;
        }
        let running = running(self);
        if let Some(key) = &self.pinned
            && !running.iter().any(|r| &r.key == key)
            && matches!(self.live, crate::live::LiveView::Ready { .. })
        {
            self.pinned = None;
        }
        let now = shown(
            &self.target.name,
            &running,
            self.waiting() + self.approvals_waiting(),
            self.pinned.as_deref(),
        );
        let last = self.tray.as_ref();
        if last.map(|s| &s.badge) != Some(&now.badge) {
            rune_mac::dock::set_badge(now.badge.as_deref());
        }
        if last.map(|s| s.progress) != Some(now.progress) {
            rune_mac::dock::set_progress(now.progress.map(|p| f64::from(p) / 1000.0));
        }
        if last.map(|s| (&s.title, &s.lines)) != Some((&now.title, &now.lines)) {
            rune_mac::status::show(&now.title, &now.lines);
        }
        self.tray = Some(now);
    }

    /// Does what the menu bar pick `id` asks.
    pub fn picked(&mut self, id: &str) {
        match pick(id) {
            Some(Pick::Open(key)) => {
                if let Some(playbook) = self.live.launch(&key).map(|l| l.id.clone()) {
                    self.follow(&crate::link::Link::Launch {
                        playbook,
                        key,
                        task: None,
                    });
                }
                crate::notify::bring_to_front();
            }
            Some(Pick::Pin(key)) => self.pinned = key,
            Some(Pick::Decisions) => {
                self.page = crate::Page::Decisions;
                crate::notify::bring_to_front();
            }
            Some(Pick::Show) => crate::notify::bring_to_front(),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use rune_mac::status::Line;

    use crate::tray::{Pick, Running, pick, shown};

    fn run(key: &str, cost: f64, cap: f64, elapsed: i64, max: Option<i64>) -> Running {
        Running {
            key: key.into(),
            playbook: format!("pb-{key}"),
            cost,
            cap,
            elapsed,
            max,
        }
    }

    #[test]
    fn the_budget_is_whichever_cap_is_closer() {
        assert_eq!(run("a", 1.0, 4.0, 60, Some(600)).budget(), Some(0.25));
        assert_eq!(run("a", 1.0, 4.0, 540, Some(600)).budget(), Some(0.9));
        assert_eq!(
            run("a", 9.0, 4.0, 0, None).budget(),
            Some(1.0),
            "never past full"
        );
        assert_eq!(
            run("a", 1.0, 0.0, 30, None).budget(),
            None,
            "no cap, no budget"
        );
    }

    #[test]
    fn a_line_says_spend_time_and_budget() {
        assert_eq!(
            run("a", 1.2, 5.0, 750, Some(1800)).line(),
            "pb-a \u{b7} $1.20 of $5.00 \u{b7} 12m of 30m \u{b7} 42%"
        );
        assert_eq!(
            run("a", 0.0, 0.0, 3900, None).line(),
            "pb-a \u{b7} $0.00 \u{b7} 1h05m"
        );
    }

    #[test]
    fn nothing_running_shows_no_progress_and_a_quiet_menu() {
        let s = shown("mpp", &[], 0, Some("gone"));
        assert_eq!(s.badge, None);
        assert_eq!(s.progress, None);
        assert_eq!(s.title, "\u{25c7}");
        assert_eq!(
            s.lines,
            [
                Line::Label("Nothing running on mpp".into()),
                Line::Separator,
                Line::item("show", "Open Crucible Playbooks"),
            ]
        );
    }

    #[test]
    fn a_pinned_launch_draws_its_budget_and_is_checked() {
        let running = [run("a", 1.0, 4.0, 0, None), run("b", 3.0, 4.0, 0, None)];
        let s = shown("mpp", &running, 2, Some("b"));
        assert_eq!(s.badge.as_deref(), Some("2"));
        assert_eq!(s.progress, Some(750));
        assert_eq!(s.title, "\u{25c6} 2");
        assert!(s.lines.contains(&Line::item("pin:b", "pb-b").checked(true)));
        assert!(s.lines.contains(&Line::item("pin:", "None").checked(false)));
        assert!(
            s.lines
                .contains(&Line::item("decisions", "2 decisions waiting"))
        );
        let opens: Vec<&Line> = s
            .lines
            .iter()
            .filter(|l| matches!(l, Line::Item { id, .. } if id.starts_with("open:")))
            .collect();
        assert_eq!(opens.len(), 2);
    }

    #[test]
    fn picks_read_back_from_their_ids() {
        assert_eq!(
            pick("open:playbook:x:1"),
            Some(Pick::Open("playbook:x:1".into()))
        );
        assert_eq!(
            pick("pin:playbook:x:1"),
            Some(Pick::Pin(Some("playbook:x:1".into())))
        );
        assert_eq!(pick("pin:"), Some(Pick::Pin(None)));
        assert_eq!(pick("decisions"), Some(Pick::Decisions));
        assert_eq!(pick("show"), Some(Pick::Show));
        assert_eq!(pick("bogus"), None);
    }
}

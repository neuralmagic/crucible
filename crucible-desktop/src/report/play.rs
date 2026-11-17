//! What a live view keeps between frames: the state its newest view returned, the event it was
//! last asked with, the events waiting behind it, and when its next tick is due.

use std::collections::{BTreeMap, VecDeque};

use crucible_view::{Event, Key, Param};

use crate::report::{Asked, Report};

/// The most events kept while the plugin answers an earlier one.
pub const QUEUED: usize = 8;

/// A live view's events, answered in order, one at a time.
#[derive(Debug, Default)]
pub struct Play {
    /// The tick the newest view asked for, in milliseconds.
    tick_ms: Option<u32>,
    /// The keys the newest view asked for.
    keys: Vec<Key>,
    /// The state the newest view built for the current turn returned.
    state: Option<String>,
    /// How many events have asked.
    turn: u64,
    /// The event of the current turn, until a view built for it arrives.
    waiting: Option<Event>,
    /// Clicks and keys that came while `waiting` was unanswered, oldest first.
    queued: VecDeque<Event>,
    /// When the next tick is due, on the window's clock in seconds.
    due: Option<f64>,
}

impl Play {
    /// What the plugin is asked with next, with the reader's `params`.
    pub fn asked(&self, params: &BTreeMap<String, Param>) -> Asked {
        Asked {
            params: params.clone(),
            turn: self.turn,
            state: self.state.clone(),
            event: self.waiting.clone(),
        }
    }

    /// Takes what `report` returned: its tick and keys, and its state when it was built for the
    /// current turn, which answers that turn's event and lets the next queued one ask.
    pub fn answer(&mut self, report: &Report) {
        let answered = report.asked.turn == self.turn;
        match &report.view {
            Ok(view) => {
                self.tick_ms = view.live.tick_ms;
                self.keys.clone_from(&view.live.keys);
                if answered {
                    self.state.clone_from(&view.live.state);
                }
                if self.tick_ms.is_none() {
                    self.due = None;
                }
            }
            Err(_) => self.rest(),
        }
        if answered {
            self.waiting = None;
            if let Some(next) = self.queued.pop_front() {
                self.fire(next);
            }
        }
    }

    /// Stops the tick and the keys and drops what waits, for a view that failed or is gone. The
    /// state, and the event being answered, stay for the view that comes next.
    pub fn rest(&mut self) {
        self.tick_ms = None;
        self.keys.clear();
        self.queued.clear();
        self.due = None;
    }

    /// Asks the plugin again with `event` once the events before it are answered. A tick is
    /// dropped while another event waits, and at most [`QUEUED`] clicks and keys wait.
    pub fn fire(&mut self, event: Event) {
        if self.waiting.is_some() {
            if event != Event::Tick && self.queued.len() < QUEUED {
                self.queued.push_back(event);
            }
            return;
        }
        self.turn += 1;
        self.waiting = Some(event);
    }

    /// Whether the newest view asked for `key`.
    pub fn wants(&self, key: Key) -> bool {
        self.keys.contains(&key)
    }

    /// Whether the newest view asked for any key.
    pub fn keyed(&self) -> bool {
        !self.keys.is_empty()
    }

    /// Fires a tick when one is due at `clock`, in seconds, and says how many seconds until the
    /// next is; `None` when the view asks for none.
    pub fn tick(&mut self, clock: f64) -> Option<f64> {
        let every = f64::from(self.tick_ms?) / 1000.0;
        let due = *self.due.get_or_insert(clock + every);
        if clock < due {
            return Some(due - clock);
        }
        self.due = Some(clock + every);
        self.fire(Event::Tick);
        Some(every)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crucible_view::{Event, Key, Live, Node, View};

    use crate::Selection;
    use crate::report::play::{Play, QUEUED};
    use crate::report::shown::Scoped;
    use crate::report::{Asked, Report};

    fn live(turn: u64, state: &str, tick_ms: Option<u32>) -> Arc<Report> {
        Arc::new(Report {
            author: None,
            asked: Asked {
                turn,
                ..Asked::default()
            },
            view: Ok(View {
                title: "T".into(),
                body: Node::Stats { items: vec![] },
                controls: vec![],
                live: Live {
                    state: Some(state.into()),
                    tick_ms,
                    keys: vec![Key::Left],
                },
            }),
        })
    }

    fn broken(turn: u64) -> Arc<Report> {
        Arc::new(Report {
            author: None,
            asked: Asked {
                turn,
                ..Asked::default()
            },
            view: Err("trapped".into()),
        })
    }

    #[test]
    fn an_event_asks_once_and_the_next_waits_for_its_answer() {
        let a = Selection::Local("stack".into());
        let mut shown = Scoped::default();
        shown.follow(Some(&a), Some(&live(0, "s0", None)));
        let first = shown.shown.asked();
        assert_eq!((first.turn, first.state.as_deref()), (0, Some("s0")));
        assert_eq!(first.event, None);
        assert!(shown.shown.play.keyed() && shown.shown.play.wants(Key::Left));
        assert!(!shown.shown.play.wants(Key::Right));

        shown.shown.play.fire(Event::Key(Key::Left));
        shown.shown.play.fire(Event::Hit("drop".into()));
        shown.shown.play.fire(Event::Tick);
        let asked = shown.shown.asked();
        assert_eq!(asked.turn, 1);
        assert_eq!(asked.event, Some(Event::Key(Key::Left)));
        assert_eq!(asked.state.as_deref(), Some("s0"));
        assert_ne!(asked, first, "an event is a new ask");

        shown.follow(Some(&a), Some(&live(0, "stale", None)));
        assert_eq!(
            shown.shown.asked().state.as_deref(),
            Some("s0"),
            "a view built before the event does not answer it"
        );
        assert_eq!(shown.shown.asked().turn, 1);

        shown.follow(Some(&a), Some(&live(1, "s1", None)));
        let next = shown.shown.asked();
        assert_eq!(next.turn, 2, "the queued click asks next");
        assert_eq!(next.event, Some(Event::Hit("drop".into())));
        assert_eq!(next.state.as_deref(), Some("s1"));

        shown.follow(Some(&a), Some(&live(2, "s2", None)));
        let rest = shown.shown.asked();
        assert_eq!(
            (rest.turn, rest.event.clone(), rest.state.as_deref()),
            (2, None, Some("s2")),
            "the tick that came while another waited was dropped"
        );
        assert_eq!(rest, next, "an answered event asks nothing more");

        for _ in 0..20 {
            shown.shown.play.fire(Event::Hit("x".into()));
        }
        assert_eq!(shown.shown.play.queued.len(), QUEUED, "only so many wait");
        assert_eq!(shown.shown.asked().turn, 3);

        let b = Selection::Local("other".into());
        shown.follow(Some(&b), None);
        let fresh = shown.shown.asked();
        assert_eq!((fresh.turn, fresh.state, fresh.event), (0, None, None));
        assert!(shown.shown.play.queued.is_empty());
        assert!(!shown.shown.play.keyed());
    }

    #[test]
    fn a_tick_fires_when_due_and_not_while_an_event_waits() {
        let a = Selection::Local("stack".into());
        let mut shown = Scoped::default();
        assert_eq!(
            shown.shown.play.tick(5.0),
            None,
            "nothing shown asks for a tick"
        );
        shown.follow(Some(&a), Some(&live(0, "s0", Some(500))));
        assert_eq!(
            shown.shown.play.tick(10.0),
            Some(0.5),
            "the first tick is one period on"
        );
        assert_eq!(shown.shown.asked().turn, 0);
        let wait = shown.shown.play.tick(10.2).unwrap();
        assert!((wait - 0.3).abs() < 1e-9, "{wait}");
        assert_eq!(shown.shown.play.tick(10.5), Some(0.5), "due on the dot");
        let asked = shown.shown.asked();
        assert_eq!((asked.turn, asked.event), (1, Some(Event::Tick)));

        assert_eq!(shown.shown.play.tick(11.0), Some(0.5));
        assert_eq!(
            shown.shown.asked().turn,
            1,
            "no second tick while the first is unanswered"
        );
        assert!(shown.shown.play.queued.is_empty(), "a tick never waits");
        shown.follow(Some(&a), Some(&live(1, "s1", Some(500))));
        assert_eq!(shown.shown.play.tick(11.5), Some(0.5));
        assert_eq!(shown.shown.asked().turn, 2);

        shown.follow(Some(&a), Some(&live(2, "over", None)));
        assert_eq!(
            shown.shown.play.tick(12.0),
            None,
            "a view that stops ticking is left alone"
        );
        assert_eq!(shown.shown.asked().turn, 2);

        shown.follow(Some(&a), Some(&live(2, "again", Some(250))));
        assert_eq!(
            shown.shown.play.tick(20.0),
            Some(0.25),
            "a new clock starts over"
        );
        assert_eq!(shown.shown.play.tick(20.25), Some(0.25));
        assert_eq!(shown.shown.asked().turn, 3);
    }

    #[test]
    fn a_view_that_fails_answers_its_event_and_drops_what_waits() {
        let a = Selection::Local("stack".into());
        let mut shown = Scoped::default();
        shown.follow(Some(&a), Some(&live(0, "s0", Some(100))));
        shown.shown.play.fire(Event::Key(Key::Left));
        shown.shown.play.fire(Event::Key(Key::Left));
        shown.shown.play.fire(Event::Hit("drop".into()));
        assert_eq!(shown.shown.play.queued.len(), 2);
        shown.follow(Some(&a), Some(&broken(1)));
        let after = shown.shown.asked();
        assert_eq!(
            (after.turn, after.event, after.state.as_deref()),
            (1, None, Some("s0")),
            "the event is answered and the last good state kept"
        );
        assert!(shown.shown.play.queued.is_empty());
        assert!(!shown.shown.play.keyed());
        assert_eq!(shown.shown.play.tick(1.0), None);

        let mut play = Play::default();
        play.fire(Event::Tick);
        play.answer(&broken(0));
        assert_eq!(play.asked(&Default::default()).event, Some(Event::Tick));
        play.answer(&broken(1));
        assert_eq!(play.asked(&Default::default()).event, None);
    }

    #[test]
    fn a_view_that_goes_away_stops_its_tick_and_keys_and_keeps_its_state() {
        let a = Selection::Local("stack".into());
        let mut shown = Scoped::default();
        let first = live(0, "s0", Some(500));
        shown.follow(Some(&a), Some(&first));
        assert_eq!(shown.shown.play.tick(1.0), Some(0.5));
        shown.shown.play.fire(Event::Hit("drop".into()));
        shown.shown.play.fire(Event::Key(Key::Left));
        shown.follow(Some(&a), None);
        assert_eq!(shown.shown.play.tick(5.0), None, "nothing to tick");
        assert!(!shown.shown.play.keyed());
        assert!(shown.shown.play.queued.is_empty());
        let asked = shown.shown.asked();
        assert_eq!(
            (asked.turn, asked.event, asked.state.as_deref()),
            (1, Some(Event::Hit("drop".into())), Some("s0")),
            "the event still waits for its answer"
        );

        shown.follow(Some(&a), Some(&first));
        assert!(shown.shown.play.keyed(), "the view is back, a turn behind");
        assert_eq!(shown.shown.asked().event, Some(Event::Hit("drop".into())));
        assert_eq!(
            shown.shown.play.tick(9.0),
            Some(0.5),
            "its clock starts over"
        );
        shown.follow(Some(&a), Some(&live(1, "s1", Some(500))));
        let answered = shown.shown.asked();
        assert_eq!(
            (answered.turn, answered.event, answered.state.as_deref()),
            (1, None, Some("s1"))
        );
    }

    #[test]
    fn the_reader_s_params_ride_along_with_the_turn() {
        let mut play = Play::default();
        let params = [(
            "lane".to_string(),
            crucible_view::Param::Select("go".into()),
        )]
        .into();
        let asked = play.asked(&params);
        assert_eq!(asked.params, params);
        assert_ne!(asked, play.asked(&Default::default()), "params are the ask");
        play.fire(Event::Tick);
        assert_ne!(asked, play.asked(&params), "so is the turn");
        let same = Asked {
            state: Some("other".into()),
            event: Some(Event::Key(Key::Up)),
            ..play.asked(&params)
        };
        assert_eq!(same, play.asked(&params), "the state and the event are not");
    }
}

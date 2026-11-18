//! Back and forward through the places the window showed: which backend, what was selected, and
//! which page.

use crate::{App, Backend, Page, Selection};

/// How many places back are kept.
const KEPT: usize = 100;

/// One place the window showed.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub backend: Backend,
    pub selection: Option<Selection>,
    pub page: Page,
}

#[derive(Default)]
pub struct History {
    back: Vec<Place>,
    forward: Vec<Place>,
    at: Option<Place>,
}

impl History {
    /// The window shows `now`: a new place ends what was forward of the last one.
    pub fn saw(&mut self, now: Place) {
        if self.at.as_ref() == Some(&now) {
            return;
        }
        if let Some(at) = self.at.replace(now) {
            self.back.push(at);
            let over = self.back.len().saturating_sub(KEPT);
            self.back.drain(..over);
        }
        self.forward.clear();
    }

    /// The place before this one, now current.
    pub fn back(&mut self) -> Option<Place> {
        let to = self.back.pop()?;
        if let Some(at) = self.at.replace(to.clone()) {
            self.forward.push(at);
        }
        Some(to)
    }

    /// The place after this one, now current.
    pub fn forward(&mut self) -> Option<Place> {
        let to = self.forward.pop()?;
        if let Some(at) = self.at.replace(to.clone()) {
            self.back.push(at);
        }
        Some(to)
    }
}

impl App {
    pub fn place(&self) -> Place {
        Place {
            backend: self.backend,
            selection: self.selection.clone(),
            page: self.page,
        }
    }

    /// Go back (`-1`) or forward (`1`) a place.
    pub fn go(&mut self, by: i32) {
        let to = if by < 0 {
            self.history.back()
        } else {
            self.history.forward()
        };
        let Some(to) = to else {
            return;
        };
        self.backend = to.backend;
        match to.selection {
            Some(selection) => self.select(selection),
            None => self.selection = None,
        }
        self.page = to.page;
        self.sync_live();
    }
}

#[cfg(test)]
mod tests {
    use crate::history::{History, Place};
    use crate::{Backend, Page, Selection};

    fn at(page: Page, playbook: Option<&str>) -> Place {
        Place {
            backend: Backend::Controller,
            selection: playbook.map(Selection::playbook),
            page,
        }
    }

    #[test]
    fn back_and_forward_walk_the_places_and_a_new_one_ends_forward() {
        let mut h = History::default();
        h.saw(at(Page::Runs, Some("a")));
        h.saw(at(Page::Runs, Some("a")));
        h.saw(at(Page::Runs, Some("b")));
        h.saw(at(Page::Triggers, Some("b")));
        assert_eq!(h.back(), Some(at(Page::Runs, Some("b"))));
        assert_eq!(h.back(), Some(at(Page::Runs, Some("a"))));
        assert_eq!(h.back(), None, "nothing before the first");
        assert_eq!(h.forward(), Some(at(Page::Runs, Some("b"))));
        h.saw(at(Page::Runs, Some("b")));
        assert_eq!(h.forward(), Some(at(Page::Triggers, Some("b"))));
        h.back();
        h.saw(at(Page::Secrets, None));
        assert_eq!(h.forward(), None, "a new place ends forward");
        assert_eq!(h.back(), Some(at(Page::Runs, Some("b"))));
    }

    #[test]
    fn only_so_many_places_are_kept() {
        let mut h = History::default();
        for i in 0..150 {
            h.saw(at(Page::Runs, Some(&format!("p{i}"))));
        }
        let mut steps = 0;
        while h.back().is_some() {
            steps += 1;
        }
        assert_eq!(steps, 100);
    }
}

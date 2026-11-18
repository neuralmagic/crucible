//! Pages and panes: the workspace layout, which pages the caller is offered, and the zoom.

use crate::live::LiveView;
use crate::workspace::{PaneKind, Workspace};
use crate::{App, Backend, Group, Page, SESSION_AUTH, workspace};

impl App {
    /// The workspace to draw: the user's own, or one fitted to the graph on screen.
    pub fn workspace(&self) -> Workspace {
        self.layout.clone().unwrap_or_else(|| {
            let tasks = self.shown_tasks();
            let mut fitted = Workspace::for_depth(workspace::depth(
                tasks
                    .iter()
                    .map(|task| (task.name.as_str(), task.depends_on.as_slice())),
            ));
            for kind in &self.shown {
                fitted.activate(*kind);
            }
            fitted
        })
    }

    /// Brings `kind` to the front of its group without making the layout the user's own.
    pub fn show_pane(&mut self, kind: PaneKind) {
        match &mut self.layout {
            Some(layout) => {
                layout.activate(kind);
                self.save_layout();
            }
            None => {
                self.shown.retain(|shown| *shown != kind);
                self.shown.push(kind);
            }
        }
    }

    /// Changes the workspace, which from then on is the user's own.
    pub fn arrange(&mut self, f: impl FnOnce(&mut Workspace)) {
        let mut layout = self.workspace();
        f(&mut layout);
        self.layout = Some(layout);
    }

    pub fn save_layout(&mut self) {
        self.settings.set_layout(self.layout.clone());
    }

    /// Back to the layout fitted to each run's graph.
    pub fn reset_layout(&mut self) {
        self.layout = None;
        self.save_layout();
    }

    /// What the signed-in user may own things as, themself first.
    /// Whether the signed-in caller administers the controller.
    pub fn is_admin(&self) -> bool {
        matches!(&*self.remote.live, LiveView::Ready { identity, .. } if identity.role == "admin")
    }

    /// Whether the signed-in caller may use the controller's own issue lane.
    pub fn has_autoresearch(&self) -> bool {
        matches!(&*self.remote.live, LiveView::Ready { identity, .. } if identity.autoresearch)
    }

    pub fn owners(&self) -> Vec<String> {
        match &*self.remote.live {
            LiveView::Ready { identity, .. } if !identity.owners.is_empty() => {
                identity.owners.clone()
            }
            _ => vec!["user:self".to_string()],
        }
    }

    /// Lay every window out at `zoom` and keep it for next time.
    pub fn set_zoom(&mut self, zoom: f32) {
        rune::zoom::set(zoom);
        self.settings.set_zoom(zoom);
    }

    /// Show `page`, or the runs when it already shows.
    pub fn toggle_page(&mut self, page: Page) {
        self.page = if self.page == page { Page::Runs } else { page };
    }

    /// Whether `page` is offered to this caller on this backend.
    pub fn offers(&self, page: Page) -> bool {
        let controller = self.backend == Backend::Controller;
        match page {
            Page::Runs | Page::Settings => true,
            Page::Issues => controller && self.has_autoresearch(),
            Page::Admin => controller && self.is_admin(),
            Page::Keys => controller && SESSION_AUTH,
            _ => controller,
        }
    }

    /// The pages of `group` offered to this caller, in tab order.
    pub fn tabs(&self, group: Group) -> Vec<Page> {
        group
            .pages()
            .iter()
            .copied()
            .filter(|page| self.offers(*page))
            .collect()
    }

    /// Open `group` on its first page, or close it when one of its pages is shown. The inbox
    /// opens on approvals when only those wait.
    pub fn toggle_group(&mut self, group: Group) {
        if self.page.group() == Some(group) {
            self.page = Page::Runs;
            return;
        }
        let approvals_only =
            group == Group::Inbox && self.waiting() == 0 && self.approvals_waiting() > 0;
        self.page = if approvals_only {
            Page::Approvals
        } else {
            self.tabs(group).first().copied().unwrap_or(Page::Runs)
        };
    }
}

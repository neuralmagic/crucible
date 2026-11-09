//! What the system opens with the app. A pack folder dropped on the Dock icon, or opened with the
//! app from Finder, is pushed as the next version of the draft named after it and test-fired on
//! the profile in use under the drop ceilings in Settings, and its launch opens. A controller web
//! link (`crucible://`) opens its view on the profile it names.

use std::path::PathBuf;

use crate::{App, Backend, Selection};

impl App {
    /// Launch each folder in `paths`; files that are not folders are ignored.
    pub fn open_paths(&mut self, paths: Vec<PathBuf>) {
        for dir in paths.into_iter().filter(|p| p.is_dir()) {
            self.switch(Backend::Controller);
            self.page = crate::Page::Runs;
            self.live_client.drop_pack(
                dir,
                self.settings.drop_max_cost,
                self.settings.drop_max_time.clone(),
            );
        }
    }

    /// Open the launch a drop started, or say why it did not.
    pub(crate) fn answer_drop(&mut self, dropped: Option<crate::live::Dropped>) {
        let Some(dropped) = dropped.filter(|d| d.seq > self.dropped_seen) else {
            return;
        };
        self.dropped_seen = dropped.seq;
        let (title, body, link) = match &dropped.result {
            Ok(key) => {
                self.select(Selection::Live {
                    playbook: dropped.draft.clone(),
                    launch: Some(key.clone()),
                });
                crate::notify::bring_to_front();
                let link = crate::link::Link::Launch {
                    playbook: dropped.draft.clone(),
                    key: key.clone(),
                    task: None,
                };
                (
                    format!("Launched {}", dropped.draft),
                    format!("on {}", self.target.name),
                    link.url(),
                )
            }
            Err(why) => {
                let title = format!("Launching {} failed", dropped.dir.display());
                eprintln!("crucible-playbooks: {title}: {why}");
                self.drop_failed = Some(format!("{title}: {why}"));
                crate::notify::bring_to_front();
                (title, why.clone(), String::new())
            }
        };
        if let Some(notifier) = &self.notifier {
            notifier.post(&crate::notify::Note {
                id: format!("drop:{}", dropped.dir.display()),
                title,
                body,
                link,
            });
        }
    }
}

impl App {
    /// Open a controller web link: switch to the profile it names, or the one on its origin, then
    /// show the view its path maps to. A link to no known profile changes nothing.
    pub(crate) fn follow_web(&mut self, place: &crate::link::Place, route: &crate::link::Route) {
        use crate::link::{Link, Place, Route};
        let usable = crate::profile::usable(&self.settings);
        let name = match place {
            Place::Profile(name) => usable.contains_key(name).then(|| name.clone()),
            Place::Origin(origin) => usable
                .iter()
                .find(|(_, p)| {
                    url::Url::parse(&p.url)
                        .is_ok_and(|u| u.origin().ascii_serialization() == *origin)
                })
                .map(|(name, _)| name.clone()),
        };
        let Some(name) = name else {
            return;
        };
        self.switch(Backend::Controller);
        self.use_profile(&name);
        match route {
            Route::Launch { playbook, key } => self.follow(&Link::Launch {
                playbook: playbook.clone(),
                key: key.clone(),
                task: None,
            }),
            Route::Playbook { id } => self.follow(&Link::Playbook { id: id.clone() }),
            Route::Decision { id } => self.follow(&Link::Decision { id: id.clone() }),
            Route::Page(page) => self.page = *page,
            Route::Home => self.page = crate::Page::Runs,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::live::draft_id;

    #[test]
    fn a_folder_name_becomes_a_draft_id() {
        let id = |p: &str| draft_id(Path::new(p));
        assert_eq!(id("/x/cve-triage-llm-d"), Some("cve-triage-llm-d".into()));
        assert_eq!(id("/x/My Pack_v2"), Some("my-pack-v2".into()));
        assert_eq!(id("/x/--weird--"), Some("weird".into()));
        assert_eq!(id("/x/___"), None);
        assert_eq!(id("/"), None);
    }
}

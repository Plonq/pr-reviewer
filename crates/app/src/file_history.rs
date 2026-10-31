//! In-session file selection history. Viewports remain in `ItemData::file_positions`;
//! history stores paths, never indices or transient display rows.

use gpui::{KeyBinding, NavigationDirection};
use std::collections::HashSet;

pub(crate) fn keybindings() -> [KeyBinding; 2] {
    [
        KeyBinding::new("cmd-[", crate::FileBack, Some("ReviewApp")),
        KeyBinding::new("cmd-]", crate::FileForward, Some("ReviewApp")),
    ]
}

#[derive(Default)]
pub(crate) struct FileHistory {
    back: Vec<String>,
    forward: Vec<String>,
}

impl FileHistory {
    pub(crate) fn record(&mut self, from: &str, to: &str) {
        if from != to {
            self.back.push(from.to_owned());
            self.forward.clear();
        }
    }

    pub(crate) fn can_go(&self, direction: NavigationDirection) -> bool {
        match direction {
            NavigationDirection::Back => !self.back.is_empty(),
            NavigationDirection::Forward => !self.forward.is_empty(),
        }
    }

    pub(crate) fn step(&mut self, direction: NavigationDirection, current: &str) -> Option<String> {
        let (from, to) = match direction {
            NavigationDirection::Back => (&mut self.back, &mut self.forward),
            NavigationDirection::Forward => (&mut self.forward, &mut self.back),
        };
        let target = from.pop()?;
        to.push(current.to_owned());
        Some(target)
    }

    /// Refreshes may remove files. Collapse the adjacent duplicates that this
    /// creates, including entries now equal to the displayed file. Reordering
    /// needs no special handling because entries are paths.
    pub(crate) fn retain<'a>(
        &mut self,
        paths: impl Iterator<Item = &'a str>,
        current: Option<&str>,
    ) {
        let paths: HashSet<_> = paths.collect();
        for stack in [&mut self.back, &mut self.forward] {
            stack.retain(|path| paths.contains(path.as_str()));
            stack.dedup();
            if stack.last().map(String::as_str) == current {
                stack.pop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use NavigationDirection::{Back, Forward};

    #[test]
    fn boundaries_repeats_and_branching() {
        let mut history = FileHistory::default();
        assert_eq!(history.step(Back, "a"), None);
        assert_eq!(history.step(Forward, "a"), None);
        history.record("a", "a");
        assert!(!history.can_go(Back));
        history.record("a", "b");
        history.record("b", "c");
        assert_eq!(history.step(Back, "c").as_deref(), Some("b"));
        history.record("b", "b");
        assert!(history.can_go(Forward), "reselecting must not branch");
        assert_eq!(history.step(Back, "b").as_deref(), Some("a"));
        assert_eq!(history.step(Back, "a"), None);
        assert_eq!(history.step(Forward, "a").as_deref(), Some("b"));
        history.record("b", "d");
        assert!(!history.can_go(Forward));
        assert_eq!(history.step(Back, "d").as_deref(), Some("b"));
        assert_eq!(history.step(Back, "b").as_deref(), Some("a"));
    }

    #[test]
    fn refresh_removes_missing_files_and_redundant_stops() {
        let mut history = FileHistory::default();
        for (from, to) in [("a", "gone"), ("gone", "a"), ("a", "b"), ("b", "c")] {
            history.record(from, to);
        }
        assert_eq!(history.step(Back, "c").as_deref(), Some("b"));
        history.retain(["b", "a"].into_iter(), Some("b"));
        assert!(!history.can_go(Forward));
        assert_eq!(history.step(Back, "b").as_deref(), Some("a"));
        assert!(!history.can_go(Back));
        history.retain(["b"].into_iter(), Some("b"));
        assert!(!history.can_go(Forward));
    }
}

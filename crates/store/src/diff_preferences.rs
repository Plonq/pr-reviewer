//! User-owned presentation memory, separate from fetched PR snapshots.
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffPreferences {
    pub hide_whitespace: Option<bool>,
    pub collapse_unchanged: Option<bool>,
    pub soft_wrap: Option<bool>,
}

impl DiffPreferences {
    /// Initial presentation for a PR; saved per-PR choices override these.
    pub const INITIAL: Self = Self {
        hide_whitespace: Some(true),
        collapse_unchanged: Some(false),
        soft_wrap: Some(false),
    };

    pub fn over(self, defaults: Self) -> Self {
        Self {
            hide_whitespace: self.hide_whitespace.or(defaults.hide_whitespace),
            collapse_unchanged: self.collapse_unchanged.or(defaults.collapse_unchanged),
            soft_wrap: self.soft_wrap.or(defaults.soft_wrap),
        }
    }
}

impl crate::Settings<'_> {
    pub fn diff_preferences(&self, pr: Option<(&str, u64)>) -> Result<DiffPreferences> {
        Ok(self
            .get(&key(pr))?
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default())
    }

    pub fn set_diff_preferences(
        &self,
        pr: Option<(&str, u64)>,
        prefs: DiffPreferences,
    ) -> Result<()> {
        self.set(&key(pr), serde_json::to_value(prefs)?)
    }
}

fn key(pr: Option<(&str, u64)>) -> String {
    match pr {
        Some((repository, number)) => format!("diff.pr.{repository}#{number}"),
        None => "diff.defaults".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_survive_reopen_and_override_defaults_without_cross_pr_leaks() {
        let dir = std::env::temp_dir().join(format!(
            "gander-prefs-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        let path = dir.join("store.db");
        let defaults = DiffPreferences {
            hide_whitespace: Some(true),
            ..Default::default()
        };
        let prefs = DiffPreferences {
            hide_whitespace: Some(false),
            collapse_unchanged: Some(true),
            soft_wrap: Some(true),
        };
        {
            let store = crate::Store::open(&path).unwrap();
            store
                .settings()
                .set_diff_preferences(None, defaults)
                .unwrap();
            store
                .settings()
                .set_diff_preferences(Some(("a/b", 1)), prefs)
                .unwrap();
            store
                .snapshots()
                .write(&crate::PrSnapshot {
                    repository: "a/b".into(),
                    number: 1,
                    ..Default::default()
                })
                .unwrap();
        }
        let store = crate::Store::open(&path).unwrap();
        let settings = store.settings();
        assert_eq!(settings.diff_preferences(None).unwrap(), defaults);
        assert_eq!(
            settings
                .diff_preferences(Some(("a/b", 1)))
                .unwrap()
                .over(defaults),
            prefs
        );
        for pr in [("a/b", 2), ("other/b", 1)] {
            assert_eq!(
                settings.diff_preferences(Some(pr)).unwrap().over(defaults),
                defaults
            );
        }
        assert_eq!(
            DiffPreferences::default()
                .collapse_unchanged
                .unwrap_or(false),
            false
        );
        assert_eq!(DiffPreferences::default().soft_wrap.unwrap_or(false), false);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn untouched_preferences_keep_inheriting_global_changes() {
        let prefs = DiffPreferences {
            soft_wrap: Some(true),
            ..Default::default()
        };
        for whitespace in [true, false] {
            let resolved = prefs.over(DiffPreferences {
                hide_whitespace: Some(whitespace),
                ..Default::default()
            });
            assert_eq!(resolved.hide_whitespace, Some(whitespace));
            assert_eq!(resolved.soft_wrap, Some(true));
            assert_eq!(resolved.collapse_unchanged, None);
        }
    }
}

//! Ranking live sessions against a project location.
//!
//! The rules encode the kickoff's identity constraints:
//! - an explicit ghostmc tag (tmux user option) is the only *verified* link;
//! - a cwd match is only a *candidate*, shown for explicit user selection;
//! - session names are never used as identity.

use crate::paths::{PathRelation, relation};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Facts about a tmux session relevant to matching.
#[derive(Debug, Clone, Default)]
pub struct SessionFacts {
    /// Value of the `@ghostmc_location` session option, if set.
    pub tagged_location: Option<String>,
    /// Current paths of every pane in the session.
    pub pane_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Association {
    /// Created by ghostmc for this location (tag verified on the live session).
    Tagged,
    /// A pane's cwd is the location itself.
    PathExact,
    /// A pane's cwd is inside the location (subdirectory, nested worktree...).
    PathDescendant,
}

impl Association {
    pub fn is_verified(self) -> bool {
        matches!(self, Association::Tagged)
    }
}

/// Classify one session against one location. Returns `None` when the
/// session has nothing to do with the location.
pub fn associate(location_id: &str, location_path: &Path, s: &SessionFacts) -> Option<Association> {
    if s.tagged_location.as_deref() == Some(location_id) {
        return Some(Association::Tagged);
    }
    if s.tagged_location.is_some() {
        // Tagged for a different location: never steal it on a path match.
        return None;
    }
    s.pane_paths
        .iter()
        .filter_map(|p| match relation(location_path, Path::new(p)) {
            PathRelation::Exact => Some(Association::PathExact),
            PathRelation::Descendant(_) => Some(Association::PathDescendant),
            _ => None,
        })
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(tag: Option<&str>, paths: &[&str]) -> SessionFacts {
        SessionFacts {
            tagged_location: tag.map(str::to_owned),
            pane_paths: paths.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn tag_beats_paths() {
        let loc = Path::new("/w/app");
        assert_eq!(
            associate("loc_1", loc, &facts(Some("loc_1"), &["/elsewhere"])),
            Some(Association::Tagged)
        );
    }

    #[test]
    fn foreign_tag_is_never_path_matched() {
        let loc = Path::new("/w/app");
        assert_eq!(
            associate("loc_1", loc, &facts(Some("loc_2"), &["/w/app"])),
            None
        );
    }

    #[test]
    fn best_pane_wins() {
        let loc = Path::new("/w/app");
        assert_eq!(
            associate(
                "loc_1",
                loc,
                &facts(None, &["/tmp", "/w/app/src", "/w/app"])
            ),
            Some(Association::PathExact)
        );
        assert_eq!(
            associate("loc_1", loc, &facts(None, &["/w/app/src"])),
            Some(Association::PathDescendant)
        );
    }

    #[test]
    fn parent_and_sibling_dirs_do_not_match() {
        let loc = Path::new("/w/app");
        assert_eq!(
            associate("loc_1", loc, &facts(None, &["/w", "/w/app2"])),
            None
        );
    }
}

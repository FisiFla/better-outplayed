//! Containment checks for every path the shell takes from somewhere it does not control.
//!
//! The clip index is a user-writable SQLite file and `config.toml` is hand-edited, so a path
//! read from either can name anything. Before the shell unlinks, removes a directory tree, or
//! cuts from such a path, it asks this module. The rule is [`ensure_within`]: the path's real
//! parent (symlinks, junctions and `..` resolved) must sit under one of the given roots, and
//! the path must be a proper child of it, never a root itself. The final component is not
//! resolved, so a symlink at the path is unlinked as a link and never followed out of the root.

use std::path::{Component, Path, PathBuf};

/// Why a path was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathRefusal {
    /// Relative, empty, or containing `..`.
    NotAbsoluteAndClean,
    /// Its parent does not exist or cannot be resolved.
    Unresolvable,
    /// Outside every allowed root (or a root itself).
    Outside,
}

impl std::fmt::Display for PathRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PathRefusal::NotAbsoluteAndClean => "the path is relative or contains `..`",
            PathRefusal::Unresolvable => "the path's directory could not be resolved",
            PathRefusal::Outside => "the path is outside the directories replayed manages",
        })
    }
}

/// `path`, resolved through its parent, if it is a proper child of one of `roots`.
///
/// Returns the resolved path (canonical parent joined with the unresolved final component).
pub fn ensure_within(path: &Path, roots: &[&Path]) -> Result<PathBuf, PathRefusal> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(PathRefusal::NotAbsoluteAndClean);
    }
    let name = path.file_name().ok_or(PathRefusal::NotAbsoluteAndClean)?;
    let parent = path
        .parent()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .ok_or(PathRefusal::Unresolvable)?;
    let inside = roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .any(|root| parent.starts_with(&root));
    if inside {
        Ok(parent.join(name))
    } else {
        Err(PathRefusal::Outside)
    }
}

/// `path` resolved all the way through, if it lies inside `root`; for a file about to be
/// written (which, unlike an unlink, follows a symlink at the final component).
///
/// `path` need not exist yet: the deepest existing ancestor is canonicalised and the missing
/// tail re-appended, so the caller can create the missing directories afterwards. A `..` is
/// refused outright rather than normalised. The returned path is the one to write to, so the
/// check and the use cannot disagree. [`ensure_within`]'s counterpart for creation.
pub fn ensure_new_file_within(root: &Path, path: &Path) -> Result<PathBuf, PathRefusal> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(PathRefusal::NotAbsoluteAndClean);
    }
    let root = root.canonicalize().map_err(|_| PathRefusal::Unresolvable)?;
    let mut existing = path;
    let mut tail = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_owned());
                existing = parent;
            }
            _ => return Err(PathRefusal::Unresolvable),
        }
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|_| PathRefusal::Unresolvable)?;
    resolved.extend(tail.iter().rev());
    if resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        Err(PathRefusal::Outside)
    }
}

/// Whether `name` is `session-<digits>` with an optional `-<digits>` suffix and, for a file, the
/// `.mp4` extension: what `replayed_recorder::session` names its directories and files. A
/// configured `sessions_dir` may be anywhere, so this shape (not the location) identifies a
/// session artifact outside the app data directory.
pub fn is_session_name(name: &str, file: bool) -> bool {
    let stem = if file {
        match name.strip_suffix(".mp4") {
            Some(stem) => stem,
            None => return false,
        }
    } else {
        name
    };
    let Some(rest) = stem.strip_prefix("session-") else {
        return false;
    };
    let mut parts = rest.splitn(2, '-');
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match (parts.next(), parts.next()) {
        (Some(a), None) => digits(a),
        (Some(a), Some(b)) => digits(a) && digits(b),
        _ => false,
    }
}

/// Whether `path` is acceptable as a session's scratch directory (`file == false`) or recorded
/// file (`file == true`) to remove or cut from: a proper child of one of `roots`, or shaped like
/// a recorder-made session artifact, and in both cases never a protected location (one of
/// `protected`, or an ancestor of one).
pub fn ensure_session_artifact(
    path: &Path,
    file: bool,
    roots: &[&Path],
    protected: &[&Path],
) -> Result<PathBuf, PathRefusal> {
    let resolved = match ensure_within(path, roots) {
        Ok(p) => p,
        Err(PathRefusal::Outside) => {
            let shaped = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| is_session_name(n, file));
            if !shaped {
                return Err(PathRefusal::Outside);
            }
            // Same resolution `ensure_within` does, without the roots test.
            ensure_within(path, &[path.parent().unwrap_or(path)])?
        }
        Err(other) => return Err(other),
    };
    let clashes = protected
        .iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .any(|p| p.starts_with(&resolved));
    if clashes {
        return Err(PathRefusal::Outside);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_is_accepted_and_the_root_itself_and_outsiders_are_not() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let inside = root.path().join("a.mp4");
        std::fs::write(&inside, b"").unwrap();
        assert!(ensure_within(&inside, &[root.path()]).is_ok());
        // A path that does not exist yet is fine: only its parent is resolved.
        assert!(ensure_within(&root.path().join("new.mp4"), &[root.path()]).is_ok());
        assert_eq!(
            ensure_within(&other.path().join("b.mp4"), &[root.path()]),
            Err(PathRefusal::Outside)
        );
        // The parent of the root is outside it, so the root itself cannot be named.
        assert_eq!(
            ensure_within(root.path(), &[root.path()]),
            Err(PathRefusal::Outside)
        );
    }

    #[test]
    fn dotdot_relative_and_unresolvable_paths_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let sneaky = root.path().join("sub").join("..").join("..").join("x");
        assert_eq!(
            ensure_within(&sneaky, &[root.path()]),
            Err(PathRefusal::NotAbsoluteAndClean)
        );
        assert_eq!(
            ensure_within(Path::new("relative/x.mp4"), &[root.path()]),
            Err(PathRefusal::NotAbsoluteAndClean)
        );
        assert_eq!(
            ensure_within(&root.path().join("missing-dir").join("x"), &[root.path()]),
            Err(PathRefusal::Unresolvable)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_parent_that_leaves_the_root_is_refused_but_a_link_at_the_path_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("precious.mp4"), b"").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("door")).unwrap();
        assert_eq!(
            ensure_within(
                &root.path().join("door").join("precious.mp4"),
                &[root.path()]
            ),
            Err(PathRefusal::Outside),
            "a directory link out of the root resolves to outside"
        );
        // A link *as* the final component stays a link: the check is on where it lives.
        std::os::unix::fs::symlink(
            outside.path().join("precious.mp4"),
            root.path().join("l.mp4"),
        )
        .unwrap();
        let resolved = ensure_within(&root.path().join("l.mp4"), &[root.path()]).unwrap();
        assert!(resolved.starts_with(std::fs::canonicalize(root.path()).unwrap()));
    }

    #[test]
    fn a_new_file_inside_the_root_is_accepted_even_under_missing_directories() {
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("a/b/c.cfg");
        let resolved = ensure_new_file_within(dir.path(), &inside).unwrap();
        assert!(resolved.starts_with(dir.path().canonicalize().unwrap()));
        assert!(resolved.ends_with("a/b/c.cfg"));
    }

    #[test]
    fn a_new_file_with_dotdot_or_outside_the_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let sneaky = dir.path().join("sub/../../escape.cfg");
        assert!(ensure_new_file_within(dir.path(), &sneaky).is_err());
        assert!(ensure_new_file_within(dir.path(), &other.path().join("x.cfg")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_behind_a_symlink_that_leaves_the_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("cfg")).unwrap();
        assert!(ensure_new_file_within(dir.path(), &dir.path().join("cfg/x.cfg")).is_err());
    }

    #[test]
    fn session_names_match_what_the_recorder_creates() {
        assert!(is_session_name("session-1700000000", false));
        assert!(is_session_name("session-1700000000-3", false));
        assert!(is_session_name("session-1700000000.mp4", true));
        assert!(!is_session_name("session-1700000000.mp4", false));
        assert!(!is_session_name("session-1700000000", true));
        for bad in [
            "session-",
            "session-x",
            "session-1-",
            "session-1-2-3",
            "Documents",
            "..",
        ] {
            assert!(!is_session_name(bad, false), "{bad}");
        }
    }

    #[test]
    fn a_session_artifact_outside_the_roots_must_look_like_one_and_never_cover_protected_dirs() {
        let app = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let shaped = elsewhere.path().join("session-1700000000");
        let other = elsewhere.path().join("Documents");
        std::fs::create_dir_all(&shaped).unwrap();
        std::fs::create_dir_all(&other).unwrap();

        assert!(ensure_session_artifact(&shaped, false, &[app.path()], &[app.path()]).is_ok());
        assert_eq!(
            ensure_session_artifact(&other, false, &[app.path()], &[app.path()]),
            Err(PathRefusal::Outside),
            "an arbitrary directory elsewhere is not a session"
        );

        // A session-shaped directory that contains a protected location is refused.
        let nested = shaped.join("clips");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            ensure_session_artifact(&shaped, false, &[app.path()], &[&nested]),
            Err(PathRefusal::Outside)
        );
    }
}

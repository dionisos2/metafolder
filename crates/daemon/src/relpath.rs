//! A repository-relative path held as **exact bytes**.
//!
//! A POSIX name is a byte string (spec-data-model "Tree names"), so a relative
//! path cannot be a `String` without losing the files the daemon exists to
//! track. It cannot simply be a `PathBuf` either: the repository's display
//! convention — the root is `""`, everything else is `/`-prefixed — is what the
//! DSL, the tree cache and every caller speak, and getting it wrong has
//! produced doubled and missing separators before. Both live here, in one
//! place, so no caller has to remember either rule.

use std::path::{Path, PathBuf};

use metafolder_core::metarecord::TreeName;

/// The components below the repository root, outermost first. Empty = the root.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct RelPath {
    components: Vec<TreeName>,
}

impl RelPath {
    /// The repository root itself.
    pub fn root() -> Self {
        Self::default()
    }

    /// A child of this path.
    pub fn child(&self, name: TreeName) -> Self {
        let mut components = self.components.clone();
        components.push(name);
        Self { components }
    }

    /// Parses a *displayed* path (`""`, `/a`, `/a/b`; a leading slash is
    /// optional so callers holding either form work). Exact for a path that is
    /// text — which every path typed or stored as text is.
    pub fn from_display(path: &str) -> Self {
        Self { components: path.split('/').filter(|c| !c.is_empty()).map(TreeName::from).collect() }
    }

    /// The name of the last component, or None at the root.
    pub fn name(&self) -> Option<&TreeName> {
        self.components.last()
    }

    /// Everything but the last component; the root's parent is the root.
    pub fn parent(&self) -> Self {
        let mut components = self.components.clone();
        components.pop();
        Self { components }
    }

    pub fn components(&self) -> &[TreeName] {
        &self.components
    }

    /// How deep below the root, so callers can order parents before children.
    pub fn depth(&self) -> usize {
        self.components.len()
    }

    pub fn is_root(&self) -> bool {
        self.components.is_empty()
    }

    /// The displayed form: `""` at the root, `/a/b` below it. Undecodable bytes
    /// show as U+FFFD, as every file manager shows them.
    pub fn display(&self) -> String {
        let mut out = String::new();
        for component in &self.components {
            out.push('/');
            out.push_str(&component.display());
        }
        out
    }

    /// The exact bytes of the whole path, `/`-separated — the form the
    /// watcher buffer persists, so an undecodable name survives a daemon
    /// restart. Round-trips through [`Self::from_bytes`].
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for component in &self.components {
            out.push(b'/');
            out.extend_from_slice(component.as_bytes());
        }
        out
    }

    /// The inverse of [`Self::to_bytes`].
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            components: bytes
                .split(|b| *b == b'/')
                .filter(|c| !c.is_empty())
                .map(|c| TreeName::from_bytes(c.to_vec()))
                .collect(),
        }
    }

    /// The absolute path on disk, byte-exact — this is what opens the file.
    pub fn to_abs(&self, root: &Path) -> PathBuf {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let mut abs = root.to_path_buf();
            for component in &self.components {
                abs.push(std::ffi::OsStr::from_bytes(component.as_bytes()));
            }
            abs
        }
        #[cfg(not(unix))]
        {
            let mut abs = root.to_path_buf();
            for component in &self.components {
                abs.push(component.display().as_ref());
            }
            abs
        }
    }
}

/// Sugar for [`RelPath::from_display`] — exact for any path that is text,
/// which every path written as a literal is.
impl From<&str> for RelPath {
    fn from(path: &str) -> Self {
        Self::from_display(path)
    }
}

/// The exact bytes of a directory entry's name.
///
/// On unix that is what the kernel gave us, untouched. Elsewhere there is no
/// byte view of an `OsStr`, so the lossy text is the best available — a
/// platform that cannot express the name cannot track it either.
pub fn file_name_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        name.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        name.to_string_lossy().into_owned().into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_root_displays_as_the_empty_path() {
        // "" is what the DSL and the tree cache use for the root; "/" is NOT a
        // synonym here, and returning one has produced empty query results.
        let root = RelPath::root();
        assert!(root.is_root());
        assert_eq!(root.display(), "");
        assert_eq!(root.depth(), 0);
        assert_eq!(root.name(), None);
    }

    #[test]
    fn test_children_are_slash_prefixed_and_never_doubled() {
        let a = RelPath::root().child(TreeName::from("a"));
        assert_eq!(a.display(), "/a");
        assert_eq!(a.child(TreeName::from("b")).display(), "/a/b");
        assert_eq!(a.depth(), 1);
    }

    #[test]
    fn test_parsing_a_displayed_path_accepts_it_with_or_without_the_slash() {
        for text in ["/a/b", "a/b"] {
            let path = RelPath::from_display(text);
            assert_eq!(path.display(), "/a/b", "parsing {text:?}");
            assert_eq!(path.depth(), 2);
        }
        assert!(RelPath::from_display("").is_root());
        assert!(RelPath::from_display("/").is_root());
    }

    #[test]
    fn test_parent_walks_up_and_stops_at_the_root() {
        let path = RelPath::from_display("/a/b");
        assert_eq!(path.parent().display(), "/a");
        assert_eq!(path.parent().parent().display(), "");
        assert!(path.parent().parent().parent().is_root());
    }

    #[test]
    fn test_an_undecodable_component_keeps_its_bytes_and_shows_a_replacement() {
        let path = RelPath::root().child(TreeName::from_bytes(b"caf\xe9.mp4".to_vec()));
        assert_eq!(path.display(), "/caf\u{FFFD}.mp4");
        assert_eq!(path.name().unwrap().as_bytes(), b"caf\xe9.mp4");
    }

    #[cfg(unix)]
    #[test]
    fn test_the_absolute_path_carries_the_exact_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let path = RelPath::root()
            .child(TreeName::from("dir"))
            .child(TreeName::from_bytes(b"caf\xe9.mp4".to_vec()));
        let abs = path.to_abs(Path::new("/repo"));
        // The displayed form would have lost the byte; the path on disk must not.
        assert_eq!(abs.as_os_str().as_bytes(), b"/repo/dir/caf\xe9.mp4");
    }

    #[test]
    fn test_the_byte_form_round_trips_including_undecodable_names() {
        for path in [
            RelPath::root(),
            RelPath::from_display("/a/b"),
            RelPath::root()
                .child(TreeName::from("dir"))
                .child(TreeName::from_bytes(b"caf\xe9.mp4".to_vec())),
        ] {
            assert_eq!(RelPath::from_bytes(&path.to_bytes()), path, "{path:?}");
        }
    }

    #[test]
    fn test_a_component_is_never_split_on_a_separator_it_does_not_have() {
        // A name cannot contain '/', so parsing round-trips exactly.
        let path = RelPath::from_display("/a b/c-d.txt");
        assert_eq!(path.components().len(), 2);
        assert_eq!(path.display(), "/a b/c-d.txt");
    }
}

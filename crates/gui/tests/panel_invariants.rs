//! Guards the `/fsraw` invariant: **a raw file is only ever loaded as media,
//! never as a document.**
//!
//! `/fsraw` serves arbitrary local files, and it accepts the session token as a
//! `?token=` query parameter (an `<img>`/`<video>` `src` cannot carry an
//! `Authorization` header). Loaded into an `<img>`, `<video>` or `<audio>`,
//! a file is *data*: even an SVG with a `<script>` in it does not execute, and
//! a malicious one gets no further than the media decoder.
//!
//! Loaded as a **document** — an `<iframe>`, `<object>`, `<embed>`, a
//! navigation — the same file becomes *code*, running in the GUI server's
//! origin. An SVG or HTML file could then read its own URL (`location.search`),
//! lift the session token out of it, and drive the whole scripting API,
//! including `POST /gui/command` and the `!` shell commands. That is a full
//! compromise from previewing a file.
//!
//! Nothing in the browser enforces the difference, so this test does: no
//! shipped panel may contain a document-loading construct. If a panel ever
//! genuinely needs one, the token must stop travelling in the URL first.

use std::path::{Path, PathBuf};

/// Constructs that turn a URL into a document rather than media.
const DOCUMENT_LOADERS: &[&str] = &[
    "iframe",
    "<object",
    "<embed",
    "window.open",
    "location.href",
    "location.assign",
    "location.replace",
];

/// The offending lines of a panel source, `(line number, text)`.
fn document_loading_violations(source: &str) -> Vec<(usize, String)> {
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let lowered = line.to_lowercase();
            DOCUMENT_LOADERS.iter().any(|construct| lowered.contains(construct))
        })
        .map(|(index, line)| (index + 1, line.trim().to_string()))
        .collect()
}

#[test]
fn test_media_elements_are_not_flagged() {
    let source = r#"
        const img = el('img', { onerror: () => placeholder('cannot load') });
        img.src = `${metafolder.guiServer}/fsraw?path=${encodeURIComponent(path)}${auth}`;
        const media = document.createElement('video');
        media.src = url;
    "#;
    assert!(
        document_loading_violations(source).is_empty(),
        "an <img>/<video> pointed at /fsraw is the sanctioned, safe form"
    );
}

#[test]
fn test_a_document_loader_is_flagged() {
    let source = r#"
        const frame = document.createElement('iframe');
        frame.src = `${metafolder.guiServer}/fsraw?path=${path}&token=${token}`;
    "#;
    let violations = document_loading_violations(source);
    assert_eq!(violations.len(), 1, "an iframe on /fsraw must be caught");
    assert!(violations[0].1.contains("iframe"));
}

#[test]
fn test_a_navigation_to_a_raw_file_is_flagged() {
    let source = "window.location.href = fsrawUrl(path);";
    assert_eq!(document_loading_violations(source).len(), 1);
}

/// The real guard: no shipped panel loads anything as a document.
#[test]
fn test_no_shipped_panel_loads_a_raw_file_as_a_document() {
    let panels = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("default-config/panel-types");
    let mut offences: Vec<String> = Vec::new();

    for file in sources(&panels) {
        let source = std::fs::read_to_string(&file).expect("panel source");
        for (line, text) in document_loading_violations(&source) {
            offences.push(format!("{}:{line}: {text}", file.display()));
        }
    }

    assert!(
        offences.is_empty(),
        "a panel loads content as a document — a file served by /fsraw would then run as \
         code in the GUI's origin and could lift the session token out of its own URL \
         (see this file's module doc). Offending lines:\n{}",
        offences.join("\n")
    );
}

/// Every `.js` and `.html` file under `dir`, recursively.
fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(sources(&path));
        } else if path
            .extension()
            .is_some_and(|extension| extension == "js" || extension == "html")
        {
            files.push(path);
        }
    }
    files
}

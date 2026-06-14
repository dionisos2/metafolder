// file panel: media/text preview of the active path from selected_paths
// (spec-gui "file panel type"). Files are streamed by the GUI server's
// /fsraw endpoint (HTTP Range supported, so audio/video can seek).
// When the active path is a directory, its contents are shown as a
// thumbnail grid the user can click into (drill-in, with a back button).

import { el } from '/__ui.js';

const { workspace, fs } = metafolder;

const IMAGE = new Set(['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'avif']);
const AUDIO = new Set(['mp3', 'ogg', 'oga', 'flac', 'wav', 'm4a', 'opus']);
const VIDEO = new Set(['mp4', 'webm', 'mkv', 'mov', 'avi']);
const TEXT = new Set([
  'txt', 'md', 'org', 'json', 'toml', 'yaml', 'yml', 'xml', 'html', 'css', 'js', 'ts',
  'rs', 'py', 'sh', 'c', 'h', 'cpp', 'java', 'log', 'csv', 'ini', 'conf',
]);
const TEXT_PREVIEW_LIMIT = 256 * 1024;

let paths = [];
let activeIndex = 0;
// Local drill-in path: set when the user clicks into a directory's
// listing, overriding the selection until it changes. null = follow the
// selection (paths[activeIndex]).
let localPath = null;

const pathBar = document.getElementById('path-bar');
const viewer = document.getElementById('viewer');

function rawUrl(path) {
  return `${metafolder.guiServer}/fsraw?path=${encodeURIComponent(path)}`;
}

function placeholder(text) {
  viewer.replaceChildren(el('p', { class: 'placeholder' }, text));
}

function parentDir(path) {
  const index = path.lastIndexOf('/');
  return index <= 0 ? '/' : path.slice(0, index);
}

// The path currently shown: the local drill-in target, or the selection.
function viewedPath() {
  return localPath ?? paths[activeIndex];
}

// Drill into a directory entry / preview a file from the listing.
function navigateInto(path) {
  localPath = path;
  rerender();
}

// Step back out of the drill-in: up one level, returning to following the
// selection once we reach the originally selected path.
function navigateBack() {
  const parent = parentDir(localPath);
  localPath = parent === paths[activeIndex] ? null : parent;
  rerender();
}

function renderPathBar() {
  pathBar.hidden = paths.length === 0 && localPath === null;
  const children = [];
  if (localPath !== null) {
    children.push(el('button', { class: 'nav', onclick: navigateBack }, '←'));
  }
  if (localPath === null && paths.length > 1) {
    // Metarecord reachable at several locations: pick which one to preview.
    const select = el(
      'select',
      {
        onchange: () => {
          activeIndex = Number(select.value);
          void renderViewer();
        },
      },
      paths.map((path, index) =>
        el('option', { value: String(index), selected: index === activeIndex }, path),
      ),
    );
    children.push(select);
  } else {
    children.push(el('span', {}, viewedPath() ?? ''));
  }
  pathBar.replaceChildren(...children);
}

let mediaSupportCache = null;

async function mediaSupport() {
  if (mediaSupportCache === null) {
    try {
      const response = await fetch(`${metafolder.guiServer}/__media-support`);
      mediaSupportCache = await response.json();
    } catch {
      // Undeterminable (GUI server unreachable): do not degrade silently.
      mediaSupportCache = { audio: true, video: true, missing: [] };
    }
  }
  return mediaSupportCache;
}

// Directory view: a thumbnail grid of the folder's entries. Directories
// and files are both clickable (drill into a subfolder, or preview a file
// inline); images render as thumbnails, everything else as a glyph.
async function renderDirectory(dir) {
  let entries;
  try {
    entries = await fs.readDir(dir);
  } catch (error) {
    placeholder(`cannot read the folder: ${error.message ?? error}`);
    return;
  }
  if (entries.length === 0) {
    placeholder('empty folder');
    return;
  }
  // Folders first, then files, each group alphabetical (readDir sorts by
  // name only).
  entries.sort((a, b) => Number(b.is_dir) - Number(a.is_dir) || a.name.localeCompare(b.name));
  const tile = (entry) => {
    const extension = (entry.name.split('.').pop() ?? '').toLowerCase();
    let thumb;
    if (entry.is_dir) {
      thumb = el('span', { class: 'glyph' }, '📁');
    } else if (IMAGE.has(extension)) {
      thumb = el('img', {
        src: rawUrl(entry.path),
        loading: 'lazy',
        onerror: (event) => event.target.replaceWith(el('span', { class: 'glyph' }, '📄')),
      });
    } else {
      thumb = el('span', { class: 'glyph' }, '📄');
    }
    return el(
      'button',
      { class: 'tile', title: entry.name, onclick: () => navigateInto(entry.path) },
      el('span', { class: 'thumb' }, thumb),
      el('span', { class: 'name' }, entry.name),
    );
  };
  viewer.replaceChildren(el('div', { class: 'dir-grid' }, entries.map(tile)));
}

async function renderViewer() {
  const path = viewedPath();
  if (!path) {
    placeholder('No file selected');
    return;
  }
  // A directory has no meaningful file preview: list its contents instead.
  let info = null;
  try {
    info = await fs.stat(path);
  } catch {
    // Unreachable/removed: fall through to the file preview, which reports
    // the load error itself.
  }
  if (info?.is_dir) {
    await renderDirectory(path);
    return;
  }
  const extension = (path.split('.').pop() ?? '').toLowerCase();
  const url = rawUrl(path);

  if (IMAGE.has(extension)) {
    viewer.replaceChildren(
      el('img', { src: url, onerror: () => placeholder('cannot load the file') }),
    );
  } else if (AUDIO.has(extension) || VIDEO.has(extension)) {
    // A media element with no usable GStreamer pipeline does not fail
    // gracefully: it crashes the WebKit web process and freezes the
    // whole GUI. Ask the GUI server first (/__media-support).
    const kind = VIDEO.has(extension) ? 'video' : 'audio';
    const support = await mediaSupport();
    if (!support[kind]) {
      placeholder(
        `media preview disabled: missing GStreamer elements: ` +
          `${support.missing.join(', ')} (install gst-plugins-good)`,
      );
      return;
    }
    viewer.replaceChildren(el(kind, { controls: true, src: url }));
  } else if (TEXT.has(extension)) {
    try {
      const response = await fetch(url, {
        headers: { range: `bytes=0-${TEXT_PREVIEW_LIMIT - 1}` },
      });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const text = await response.text();
      const pre = el('pre', {}, text);
      viewer.replaceChildren(pre);
      if (response.status === 206 && text.length >= TEXT_PREVIEW_LIMIT) {
        pre.textContent += '\n… (truncated preview)';
      }
    } catch (error) {
      placeholder(`cannot load the file: ${error.message ?? error}`);
    }
  } else {
    placeholder('no preview available for this format');
  }
}

function update(newPaths) {
  paths = Array.isArray(newPaths) ? newPaths : [];
  activeIndex = 0;
  localPath = null;
  metafolder.whenVisible(deferredRender);
}

function rerender() {
  renderPathBar();
  void renderViewer();
}

// Loading the file (image/media/text fetch) waits for an actual display:
// a hidden instance following selected_paths must not stream anything.
const deferredRender = rerender;

await metafolder.ready;
workspace.onChange('selected_paths', update);
update(await workspace.get('selected_paths'));

// file panel: media/text preview of the active path from selected_paths
// (spec-gui "file panel type"). Files are streamed by the GUI server's
// /fsraw endpoint (HTTP Range supported, so audio/video can seek).
// When the active path is a directory, its contents are shown as a
// thumbnail grid the user can click into (drill-in, with a back button).

import { el } from '/__ui.js';

const IMAGE = new Set(['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'avif']);
const AUDIO = new Set(['mp3', 'ogg', 'oga', 'flac', 'wav', 'm4a', 'opus', 'wma', 'aac']);
const VIDEO = new Set([
  'mp4', 'webm', 'mkv', 'mov', 'avi', 'wmv', 'm4v', 'mpg', 'mpeg', 'flv', '3gp', 'ts', 'm2ts',
]);
const TEXT = new Set([
  'txt', 'md', 'org', 'json', 'toml', 'yaml', 'yml', 'xml', 'html', 'css', 'js', 'ts',
  'rs', 'py', 'sh', 'c', 'h', 'cpp', 'java', 'log', 'csv', 'ini', 'conf',
]);
const TEXT_PREVIEW_LIMIT = 256 * 1024;

export async function mount(root, metafolder) {
  const { workspace, fs } = metafolder;

  let paths = [];
  let activeIndex = 0;
  // Local drill-in path: set when the user clicks into a directory's listing,
  // overriding the selection until it changes. null = follow the selection.
  let localPath = null;
  // Bumped on every renderViewer() call so a previous render's pending async
  // work (notably the media stall timer / probe) cannot write into the viewer
  // after the shown path has changed.
  let renderGeneration = 0;

  const pathBar = root.getElementById('path-bar');
  const viewer = root.getElementById('viewer');

  function rawUrl(path) {
    return `${metafolder.guiServer}/fsraw?path=${encodeURIComponent(path)}`;
  }

  function placeholder(text) {
    viewer.replaceChildren(el('p', { class: 'placeholder' }, text));
  }

  // The currently mounted <audio>/<video>, kept so it can be torn down when
  // the view changes. A detached media element keeps its decode/network
  // pipeline alive until GC; without an explicit teardown the old stream goes
  // on buffering (in a device-less environment the decoder queue grows
  // unbounded — gigabytes of RAM, then a crash). pause + drop src + load()
  // releases it immediately.
  let activeMedia = null;

  function teardownMedia() {
    if (!activeMedia) return;
    try {
      activeMedia.pause();
    } catch {
      // Not all states allow pause(); ignore.
    }
    activeMedia.removeAttribute('src');
    try {
      activeMedia.load();
    } catch {
      // load() may throw on a removed element; the src is already gone.
    }
    activeMedia = null;
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

  // Ask the GUI server which decoders/demuxers this specific file needs and
  // lacks (runs gst-discoverer, which parses the file safely without building
  // the full decode pipeline). Returns null when the probe is unreachable.
  async function probeFile(path) {
    try {
      const response = await fetch(
        `${metafolder.guiServer}/__media-probe?path=${encodeURIComponent(path)}`,
      );
      if (response.ok) return await response.json();
    } catch {
      // Unreachable: caller decides how to proceed.
    }
    return null;
  }

  // Render an <audio>/<video>. The probe runs *before* the element is created:
  // in a GPU-less / minimal environment, building a GStreamer pipeline for a
  // file whose codec or demuxer is missing crashes the whole WebKit web
  // process (freezing the app for several seconds), so a reactive "create then
  // diagnose on error" would be too late — we must not create the element at
  // all in that case. gst-discoverer is safe; it reports the missing plugins
  // without ever building the decode pipeline that crashes WebKit.
  async function renderMedia(kind, path, url, generation) {
    const current = () => generation === renderGeneration;
    const support = await mediaSupport();
    if (!current()) return;
    if (!support[kind]) {
      placeholder(
        `media preview disabled: missing GStreamer elements: ` +
          `${support.missing.join(', ')} (install gst-plugins-good)`,
      );
      return;
    }
    placeholder('checking media support…');
    const probe = await probeFile(path);
    if (!current()) return;
    if (probe && probe.missing.length > 0) {
      placeholder(
        `cannot play this file: missing GStreamer plugin(s): ${probe.missing.join(', ')} ` +
          `(try gst-libav / gst-plugins-bad / gst-plugins-ugly)`,
      );
      return;
    }
    // `preload="metadata"`: a preview does not autoplay, so fetch only enough
    // for the first frame and duration — never buffer/decode the whole stream.
    const media = el(kind, { controls: true, preload: 'metadata', src: url });
    activeMedia = media;
    // The probe found nothing missing, but keep a light fallback for other
    // runtime failures (a corrupt stream, an unreadable file).
    media.addEventListener('error', () => {
      if (current()) placeholder('cannot play this file (unsupported or corrupt media)');
    });
    viewer.replaceChildren(media);
  }

  // Directory view: a thumbnail grid of the folder's entries.
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
    // Folders first, then files, each group alphabetical.
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
    const generation = ++renderGeneration;
    // Release any media from the previous view before showing the next one.
    teardownMedia();
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
      // Unreachable/removed: fall through to the file preview.
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
      await renderMedia(VIDEO.has(extension) ? 'video' : 'audio', path, url, generation);
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

  function rerender() {
    renderPathBar();
    void renderViewer();
  }

  // Loading the file (image/media/text fetch) waits for an actual display:
  // a hidden instance following selected_paths must not stream anything.
  function update(newPaths) {
    paths = Array.isArray(newPaths) ? newPaths : [];
    activeIndex = 0;
    localPath = null;
    metafolder.whenVisible(rerender);
  }

  workspace.onChange('selected_paths', update);
  update(await workspace.get('selected_paths'));

  // Release the media pipeline if the panel is unmounted (workspace closed or
  // panel type switched) so it cannot keep decoding in the background.
  return () => teardownMedia();
}

// file panel: media/text preview of the active path from selected_paths
// (spec-gui "file panel type"). Files are streamed by the GUI server's
// /fsraw endpoint (HTTP Range supported, so audio/video can seek).
// When the active path is a directory, its contents are shown as a
// thumbnail grid the user can click into (drill-in, with a back button).

import { el, thumbnail } from '/__ui.js';
import { createPagedList } from '/__paged-list.js';

// Directory grid: render this many thumbnails per window (more on scroll), so
// opening a folder with thousands of files does not build/load them all at
// once. The size comes from the GUI config (`[page-size].file`); this is only
// the fallback when no config value is provided.
const DIR_PAGE_DEFAULT = 150;

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

// Zoom: a step multiplies the current scale; bounds keep the manual size sane.
const ZOOM_STEP = 1.25;
const ZOOM_MIN = 0.05;
const ZOOM_MAX = 40;

export async function mount(root, metafolder) {
  const { workspace, fs, commands } = metafolder;
  const dirPage = metafolder.pageSize ?? DIR_PAGE_DEFAULT;

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
  const dirFooter = root.getElementById('dir-footer');
  const mediaToolbar = root.getElementById('media-toolbar');
  const zoomLabel = root.getElementById('zoom-label');

  // Zoom state, kept across files so a chosen level persists while browsing.
  // 'fit' fills the available box (aspect preserved); 'manual' shows the media
  // at `zoomFactor` × its natural pixel size. zoomTarget is the currently
  // zoomable <img>/<video>, or null when the view is text/audio/a directory.
  let zoomMode = 'fit';
  let zoomFactor = 1;
  let zoomTarget = null;
  // Detaches the current directory grid's scroll listener; called before the
  // next render so an old folder's pager cannot keep firing.
  let detachDirScroll = null;

  function rawUrl(path) {
    return `${metafolder.guiServer}/fsraw?path=${encodeURIComponent(path)}`;
  }

  function placeholder(text) {
    clearZoomTarget();
    viewer.replaceChildren(el('p', { class: 'placeholder' }, text));
  }

  // --- Zoom -----------------------------------------------------------------

  function naturalSize(elem) {
    if (elem.tagName === 'VIDEO') return { w: elem.videoWidth, h: elem.videoHeight };
    return { w: elem.naturalWidth, h: elem.naturalHeight };
  }

  // Run fn once the media reports its intrinsic dimensions (needed before a
  // manual pixel size can be computed).
  function onceReady(elem, fn) {
    elem.addEventListener(elem.tagName === 'VIDEO' ? 'loadedmetadata' : 'load', fn, {
      once: true,
    });
  }

  function updateToolbar() {
    mediaToolbar.hidden = zoomTarget === null;
    if (zoomTarget === null) return;
    zoomLabel.textContent = zoomMode === 'fit' ? 'Fit' : `${Math.round(zoomFactor * 100)}%`;
  }

  function applyZoom() {
    const elem = zoomTarget;
    if (!elem) return;
    elem.classList.toggle('zoom-fit', zoomMode === 'fit');
    elem.classList.toggle('zoom-manual', zoomMode === 'manual');
    if (zoomMode === 'fit') {
      elem.style.width = '';
      elem.style.height = '';
    } else {
      const nat = naturalSize(elem);
      if (!nat.w || !nat.h) {
        // Dimensions not loaded yet: re-apply when they arrive.
        onceReady(elem, applyZoom);
        return;
      }
      elem.style.width = `${Math.round(nat.w * zoomFactor)}px`;
      elem.style.height = `${Math.round(nat.h * zoomFactor)}px`;
    }
    updateToolbar();
  }

  // Make `elem` the zoom target and render it at the current zoom state.
  function setZoomTarget(elem) {
    zoomTarget = elem;
    applyZoom();
  }

  function clearZoomTarget() {
    zoomTarget = null;
    if (mediaToolbar) mediaToolbar.hidden = true;
  }

  // The on-screen scale of the target right now (used to start a manual zoom
  // continuously from whatever "fit" is currently showing).
  function shownScale(elem) {
    const nat = naturalSize(elem);
    if (!nat.w) return zoomFactor;
    return elem.getBoundingClientRect().width / nat.w;
  }

  function zoomBy(mult) {
    if (!zoomTarget) return;
    if (zoomMode === 'fit') zoomFactor = shownScale(zoomTarget) || 1;
    zoomMode = 'manual';
    zoomFactor = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, zoomFactor * mult));
    applyZoom();
  }

  function zoomFit() {
    if (!zoomTarget) return;
    zoomMode = 'fit';
    applyZoom();
  }

  function zoomReset() {
    if (!zoomTarget) return;
    zoomMode = 'manual';
    zoomFactor = 1;
    applyZoom();
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
    // Only video is zoomable; audio has no visual frame.
    if (kind === 'video') setZoomTarget(media);
  }

  // Directory view: a thumbnail grid of the folder's entries, rendered in
  // windows of `dirPage` (more appended as the grid is scrolled) so a huge
  // folder neither freezes on open nor holds thousands of <img> at once.
  async function renderDirectory(dir, generation) {
    let entries;
    try {
      entries = await fs.readDir(dir);
    } catch (error) {
      placeholder(`cannot read the folder: ${error.message ?? error}`);
      return;
    }
    // The view may have changed while readDir was in flight; bail before
    // touching the viewer so a stale folder cannot overwrite the current one.
    if (generation !== renderGeneration) return;
    if (entries.length === 0) {
      placeholder('empty folder');
      return;
    }
    // Folders first, then files, each group alphabetical.
    entries.sort((a, b) => Number(b.is_dir) - Number(a.is_dir) || a.name.localeCompare(b.name));
    const tile = (entry) =>
      el(
        'button',
        { class: 'tile', title: entry.name, onclick: () => navigateInto(entry.path) },
        el(
          'span',
          { class: 'thumb' },
          thumbnail(metafolder.guiServer, entry.path, { isDir: entry.is_dir, glyphClass: 'glyph' }),
        ),
        el('span', { class: 'name' }, entry.name),
      );

    let rendered = 0;
    const grid = el('div', { class: 'dir-grid' });
    const appendWindow = async () => {
      const next = Math.min(entries.length, rendered + dirPage);
      grid.append(...entries.slice(rendered, next).map(tile));
      rendered = next;
      dirFooter.textContent = pager.footerText();
    };
    const pager = createPagedList({
      loaded: () => rendered,
      total: () => entries.length,
      loadMore: appendWindow,
    });
    viewer.replaceChildren(grid);
    await appendWindow(); // first window
    dirFooter.hidden = false;
    detachDirScroll = pager.attach(grid);
  }

  async function renderViewer() {
    const generation = ++renderGeneration;
    // Release any media from the previous view before showing the next one.
    teardownMedia();
    // Tear down any previous directory grid (scroll listener + footer); only
    // the directory branch re-enables them.
    if (detachDirScroll) {
      detachDirScroll();
      detachDirScroll = null;
    }
    dirFooter.hidden = true;
    // Any zoomable media from the previous view is gone; the image/video
    // branches below re-arm it.
    clearZoomTarget();
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
      await renderDirectory(path, generation);
      return;
    }
    const extension = (path.split('.').pop() ?? '').toLowerCase();
    const url = rawUrl(path);

    if (IMAGE.has(extension)) {
      const img = el('img', { src: url, onerror: () => placeholder('cannot load the file') });
      viewer.replaceChildren(img);
      setZoomTarget(img);
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

  // Zoom commands (also reachable from the command input / a keybinding) and
  // their toolbar buttons. The handlers no-op unless an image/video is shown.
  commands.register('file:zoom-in', {
    label: 'File: zoom in',
    handler: () => zoomBy(ZOOM_STEP),
  });
  commands.register('file:zoom-out', {
    label: 'File: zoom out',
    handler: () => zoomBy(1 / ZOOM_STEP),
  });
  commands.register('file:zoom-fit', {
    label: 'File: fit to the available space',
    handler: zoomFit,
  });
  commands.register('file:zoom-reset', {
    label: 'File: original size',
    handler: zoomReset,
  });

  metafolder.addKeybinding('file:zoom-in', 'plus');
  metafolder.addKeybinding('file:zoom-out', '-');
  metafolder.addKeybinding('file:zoom-fit', '=');
  metafolder.addKeybinding('file:zoom-reset', '0');

  root.getElementById('zoom-in').addEventListener('click', () => zoomBy(ZOOM_STEP));
  root.getElementById('zoom-out').addEventListener('click', () => zoomBy(1 / ZOOM_STEP));
  root.getElementById('zoom-fit').addEventListener('click', zoomFit);
  root.getElementById('zoom-reset').addEventListener('click', zoomReset);

  workspace.onChange('selected_paths', update);
  update(await workspace.get('selected_paths'));

  // Release the media pipeline if the panel is unmounted (workspace closed or
  // panel type switched) so it cannot keep decoding in the background.
  return () => {
    teardownMedia();
    if (detachDirScroll) detachDirScroll();
  };
}

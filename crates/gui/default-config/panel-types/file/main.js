// file panel: media/text preview of the active path from selected_paths
// (spec-gui "file panel type"). Files are streamed by the GUI server's
// /fsraw endpoint (HTTP Range supported, so audio/video can seek).
// When the active path is a directory, its contents are shown as a
// thumbnail grid the user can click into (drill-in, with a back button).

import { byId, el, thumbnail } from '/__ui.js';
import { createPagedList } from '/__paged-list.js';
import {
  SAVE_INTERVAL_MS,
  MIN_DELTA,
  playbackAction,
  resumeTarget,
  formatPosition,
  loadPosition,
  savePosition,
  clearPosition,
} from './playback.js';

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

/**
 * The metarecord the selection points at, as metarecord-list publishes it.
 * @typedef {{uuid: string, repo: string}} Selected
 *
 * Position tracking of the mounted <video>: `stored` mirrors the field's value
 * so a redundant write can be skipped.
 * @typedef {{media: HTMLMediaElement, repo: string, uuid: string,
 *            stored: number|null, timer: ReturnType<typeof setInterval>|null}} Playback
 *
 * @param {ShadowRoot} root @param {MetafolderApi} metafolder
 */
export async function mount(root, metafolder) {
  const { workspace, fs, commands, daemon } = metafolder;
  const dirPage = metafolder.pageSize ?? DIR_PAGE_DEFAULT;

  /** @type {string[]} */
  let paths = [];
  let activeIndex = 0;
  // The metarecord the selection points at ({ uuid, repo }), published by
  // metarecord-list alongside selected_paths. It is what the playback position
  // is stored on — so it only applies while the viewer follows the selection
  // (a drill-in path has no metarecord in hand).
  /** @type {Selected|null} */
  let selected = null;
  /** @type {string|null} Local drill-in path: set when the user clicks into a
   *  directory's listing, overriding the selection until it changes.
   *  null = follow the selection. */
  let localPath = null;
  // Bumped on every renderViewer() call so a previous render's pending async
  // work (notably the media stall timer / probe) cannot write into the viewer
  // after the shown path has changed.
  let renderGeneration = 0;

  const pathBar = byId(root, 'path-bar');
  const viewer = byId(root, 'viewer');
  const dirFooter = byId(root, 'dir-footer');
  const mediaToolbar = byId(root, 'media-toolbar');
  const zoomLabel = byId(root, 'zoom-label');
  const resumeHint = byId(root, 'resume-hint');
  const gifAnimateWrap = byId(root, 'gif-animate-wrap');
  const gifAnimateBox = byId(root, 'gif-animate', HTMLInputElement);

  // GIFs are shown as a still of their first frame unless this is on (the
  // toolbar checkbox, visible only while a GIF is previewed).
  let animateGifs = false;
  /** @type {string|null} Blob URL of the current still frame, revoked when the
   *  view moves on. */
  let staticGifUrl = null;

  function revokeStaticGif() {
    if (staticGifUrl !== null) URL.revokeObjectURL(staticGifUrl);
    staticGifUrl = null;
  }

  // Still copy of an animated image: createImageBitmap uses the format's
  // default (first) frame, drawn onto a canvas and served as a blob URL — an
  // ordinary <img> for the zoom machinery, minus the animation.
  /** @param {string} url @returns {Promise<string>} a blob URL */
  async function staticFirstFrame(url) {
    const response = await fetch(url);
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const bitmap = await createImageBitmap(await response.blob());
    const canvas = document.createElement('canvas');
    canvas.width = bitmap.width;
    canvas.height = bitmap.height;
    canvas.getContext('2d')?.drawImage(bitmap, 0, 0);
    bitmap.close();
    /** @type {Blob|null} */
    const blob = await new Promise((resolve) => canvas.toBlob(resolve));
    if (!blob) throw new Error('cannot rasterize the image');
    return URL.createObjectURL(blob);
  }

  // Zoom state, kept across files so a chosen level persists while browsing.
  // 'fit' fills the available box (aspect preserved); 'manual' shows the media
  // at `zoomFactor` × its natural pixel size. zoomTarget is the currently
  // zoomable <img>/<video>, or null when the view is text/audio/a directory.
  let zoomMode = 'fit';
  let zoomFactor = 1;
  /** @type {HTMLImageElement|HTMLVideoElement|null} */
  let zoomTarget = null;
  /** @type {(() => void)|null} Detaches the current directory grid's scroll
   *  listener; called before the next render so an old folder's pager cannot
   *  keep firing. */
  let detachDirScroll = null;

  /** @param {string} path */
  function rawUrl(path) {
    const auth = metafolder.sessionToken
      ? `&token=${encodeURIComponent(metafolder.sessionToken)}`
      : '';
    return `${metafolder.guiServer}/fsraw?path=${encodeURIComponent(path)}${auth}`;
  }

  /** @param {string} text */
  function placeholder(text) {
    clearZoomTarget();
    viewer.replaceChildren(el('p', { class: 'placeholder' }, text));
  }

  // --- Zoom -----------------------------------------------------------------

  /** @param {HTMLImageElement|HTMLVideoElement} elem */
  function naturalSize(elem) {
    return elem instanceof HTMLVideoElement
      ? { w: elem.videoWidth, h: elem.videoHeight }
      : { w: elem.naturalWidth, h: elem.naturalHeight };
  }

  // Run fn once the media reports its intrinsic dimensions (needed before a
  // manual pixel size can be computed).
  /** @param {HTMLImageElement|HTMLVideoElement} elem @param {() => void} fn */
  function onceReady(elem, fn) {
    elem.addEventListener(elem instanceof HTMLVideoElement ? 'loadedmetadata' : 'load', fn, {
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
  /** @param {HTMLImageElement|HTMLVideoElement} elem */
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
  /** @param {HTMLImageElement|HTMLVideoElement} elem */
  function shownScale(elem) {
    const nat = naturalSize(elem);
    if (!nat.w) return zoomFactor;
    return elem.getBoundingClientRect().width / nat.w;
  }

  /** @param {number} mult */
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
  /** @type {HTMLMediaElement|null} */
  let activeMedia = null;

  // --- Playback position ------------------------------------------------
  //
  // Where the user stopped watching, kept on the metarecord of the played
  // file so the next open resumes there (spec-gui "file panel type"). The
  // state of the video currently mounted, or null when none is (or when it
  // has no metarecord to store a position on): { media, repo, uuid, stored,
  // timer }, with `stored` mirroring the field's value so a redundant write
  // can be skipped.
  /** @type {Playback|null} */
  let playback = null;
  // A write in flight: pause/seek/interval can all fire while one is running,
  // and each write is an event-log revision — never stack them.
  let saving = false;

  /** @param {number|null} seconds */
  function showResumeHint(seconds) {
    resumeHint.hidden = seconds === null;
    resumeHint.textContent = seconds === null ? '' : `↩ ${formatPosition(seconds)}`;
    resumeHint.title =
      seconds === null ? '' : `Playback resumes at ${formatPosition(seconds)} (last stop)`;
  }

  // Persist the position of `state`'s video. Reads currentTime/duration
  // synchronously, so it is safe to call from teardown (which drops the src
  // right after); the write itself finishes in the background.
  /** @param {Playback|null} state */
  async function persistPlayback(state) {
    if (!state || saving) return;
    const { media, repo, uuid } = state;
    const position = media.currentTime;
    const action = playbackAction(position, media.duration);
    if (action === 'none') return;
    if (action === 'clear' && state.stored === null) return; // nothing stored: no revision to open
    if (
      action === 'save' &&
      state.stored !== null &&
      Math.abs(position - state.stored) < MIN_DELTA
    ) {
      return; // unmoved (the resume seek itself lands here): not worth a revision
    }
    saving = true;
    try {
      if (action === 'clear') {
        state.stored = await clearPosition(daemon, repo, uuid);
      } else {
        const written = await savePosition(daemon, repo, uuid, position);
        if (written !== null) state.stored = written;
      }
    } finally {
      saving = false;
    }
  }

  // Arm position tracking on a freshly mounted <video>: seek to the stored
  // position, then keep it up to date. Writes happen on pause/seek/end, on
  // teardown, and periodically while playing — never on `timeupdate` (which
  // fires several times a second, and every write is an event-log revision).
  /** @param {HTMLMediaElement} media @param {number} generation */
  async function attachPlayback(media, generation) {
    if (localPath !== null || selected === null) return; // no metarecord to store it on
    const { uuid, repo } = selected;
    const stored = await loadPosition(daemon, repo, uuid);
    if (generation !== renderGeneration) return; // the view moved on while we read
    /** @type {Playback} */
    const state = { media, repo, uuid, stored, timer: null };
    playback = state;

    if (stored !== null) {
      const seek = () => {
        const target = resumeTarget(stored, media.duration);
        if (target === null) return;
        media.currentTime = target;
        showResumeHint(target);
      };
      if (media.readyState >= 1) seek(); // metadata already in: seek now
      else media.addEventListener('loadedmetadata', seek, { once: true });
    }

    const persist = () => void persistPlayback(state);
    media.addEventListener('pause', persist);
    media.addEventListener('seeked', persist);
    media.addEventListener('ended', persist);
    // Bounds what an abruptly killed window can lose; a paused video needs no
    // periodic write (the `pause` handler already stored its position).
    state.timer = setInterval(() => {
      if (!media.paused) persist();
    }, SAVE_INTERVAL_MS);
  }

  function teardownPlayback() {
    if (!playback) return;
    if (playback.timer !== null) clearInterval(playback.timer);
    void persistPlayback(playback); // fire and forget: the write outlives the element
    playback = null;
    showResumeHint(null);
  }

  function teardownMedia() {
    teardownPlayback();
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

  /** @param {string} path */
  function parentDir(path) {
    const index = path.lastIndexOf('/');
    return index <= 0 ? '/' : path.slice(0, index);
  }

  // The path currently shown: the local drill-in target, or the selection.
  /** @returns {string|undefined} */
  function viewedPath() {
    return localPath ?? paths[activeIndex];
  }

  // Drill into a directory entry / preview a file from the listing.
  /** @param {string} path */
  function navigateInto(path) {
    localPath = path;
    rerender();
  }

  // Step back out of the drill-in: up one level, returning to following the
  // selection once we reach the originally selected path.
  function navigateBack() {
    if (localPath === null) return;
    const parent = parentDir(localPath);
    localPath = parent === paths[activeIndex] ? null : parent;
    rerender();
  }

  function renderPathBar() {
    pathBar.hidden = paths.length === 0 && localPath === null;
    /** @type {HTMLElement[]} */
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

  /** @type {{audio: boolean, video: boolean, missing: string[]}|null} */
  let mediaSupportCache = null;

  async function mediaSupport() {
    if (mediaSupportCache === null) {
      try {
        const response = await fetch(`${metafolder.guiServer}/__media-support`);
        mediaSupportCache = /** @type {{audio: boolean, video: boolean, missing: string[]}} */ (
          await response.json()
        );
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
  /** @param {string} path @returns {Promise<{missing: string[]}|null>} */
  async function probeFile(path) {
    try {
      const auth = metafolder.sessionToken
        ? `&token=${encodeURIComponent(metafolder.sessionToken)}`
        : '';
      const response = await fetch(
        `${metafolder.guiServer}/__media-probe?path=${encodeURIComponent(path)}${auth}`,
      );
      if (response.ok) return /** @type {{missing: string[]}} */ (await response.json());
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
  /**
   * @param {'audio'|'video'} kind @param {string} path @param {string} url
   * @param {number} generation
   */
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
    // Only video is zoomable; audio has no visual frame. (Narrowing `kind` does
    // not narrow `media`, hence the instanceof.)
    if (media instanceof HTMLVideoElement) {
      setZoomTarget(media);
      await attachPlayback(media, generation);
    }
  }

  // Directory view: a thumbnail grid of the folder's entries, rendered in
  // windows of `dirPage` (more appended as the grid is scrolled) so a huge
  // folder neither freezes on open nor holds thousands of <img> at once.
  /** @param {string} dir @param {number} generation */
  async function renderDirectory(dir, generation) {
    /** @type {Metafolder.FsEntry[]} */
    let entries;
    try {
      entries = await fs.readDir(dir);
    } catch (error) {
      placeholder(`cannot read the folder: ${messageOf(error)}`);
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
    /** @param {Metafolder.FsEntry} entry */
    const tile = (entry) =>
      el(
        'button',
        { class: 'tile', title: entry.name, onclick: () => navigateInto(entry.path) },
        el(
          'span',
          { class: 'thumb' },
          thumbnail(metafolder.guiServer, entry.path, {
            isDir: entry.is_dir,
            glyphClass: 'glyph',
            token: metafolder.sessionToken,
          }),
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
    // branches below re-arm it. The GIF checkbox only applies to the GIF
    // branch, which un-hides it.
    clearZoomTarget();
    gifAnimateWrap.hidden = true;
    revokeStaticGif();
    const path = viewedPath();
    if (!path) {
      placeholder('No file selected');
      return;
    }
    // A directory has no meaningful file preview: list its contents instead.
    /** @type {{is_dir?: boolean}|null} */
    let info = null;
    try {
      info = /** @type {{is_dir?: boolean}} */ (await fs.stat(path));
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
      const gif = extension === 'gif';
      gifAnimateWrap.hidden = !gif;
      const img = el('img', { onerror: () => placeholder('cannot load the file') });
      if (gif && !animateGifs) {
        try {
          const still = await staticFirstFrame(url);
          if (generation !== renderGeneration) {
            URL.revokeObjectURL(still);
            return;
          }
          staticGifUrl = still;
          img.src = still;
        } catch {
          img.src = url; // cannot rasterize: fall back to the animated original
        }
      } else {
        img.src = url;
      }
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
        placeholder(`cannot load the file: ${messageOf(error)}`);
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
  /** @param {unknown} newPaths the `selected_paths` workspace variable */
  async function update(newPaths) {
    paths = Array.isArray(newPaths) ? newPaths : [];
    activeIndex = 0;
    localPath = null;
    // Read the metarecord that goes with these paths: every panel that
    // publishes selected_paths sets selected_metarecord first (null when the
    // file is untracked), so the canonical state already holds the matching
    // one. Reading it here — rather than watching it — keeps the pair
    // together: a panel that later moves selected_metarecord alone (a ref
    // click in metarecord-detail) must not re-target the played file's
    // position onto another metarecord.
    selected = /** @type {Selected|null} */ ((await workspace.get('selected_metarecord')) ?? null);
    metafolder.whenVisible(rerender);
  }

  // Zoom commands (also reachable from the command input / a keybinding) and
  // their toolbar buttons. The handlers no-op unless an image/video is shown.
  void commands.register('file:zoom-in', {
    label: 'File: zoom in',
    handler: () => zoomBy(ZOOM_STEP),
  });
  void commands.register('file:zoom-out', {
    label: 'File: zoom out',
    handler: () => zoomBy(1 / ZOOM_STEP),
  });
  void commands.register('file:zoom-fit', {
    label: 'File: fit to the available space',
    handler: zoomFit,
  });
  void commands.register('file:zoom-reset', {
    label: 'File: original size',
    handler: zoomReset,
  });

  /** @param {boolean} on */
  function setAnimateGifs(on) {
    animateGifs = on;
    gifAnimateBox.checked = on;
    void renderViewer();
  }
  void commands.register('file:toggle-gif-animation', {
    label: 'File: play/freeze GIF animations',
    handler: () => setAnimateGifs(!animateGifs),
  });
  gifAnimateBox.addEventListener('change', () => setAnimateGifs(gifAnimateBox.checked));

  // Keybindings for this panel live in keybindings.toml (when = "file").

  byId(root, 'zoom-in').addEventListener('click', () => zoomBy(ZOOM_STEP));
  byId(root, 'zoom-out').addEventListener('click', () => zoomBy(1 / ZOOM_STEP));
  byId(root, 'zoom-fit').addEventListener('click', zoomFit);
  byId(root, 'zoom-reset').addEventListener('click', zoomReset);

  workspace.onChange('selected_paths', (value) => void update(value));
  await update(await workspace.get('selected_paths'));

  // Release the media pipeline if the panel is unmounted (workspace closed or
  // panel type switched) so it cannot keep decoding in the background.
  return () => {
    teardownMedia();
    revokeStaticGif();
    if (detachDirScroll) detachDirScroll();
  };
}

/** The message of a thrown error (the fs/daemon seams throw Error). */
function messageOf(/** @type {unknown} */ error) {
  return error instanceof Error ? error.message : String(error);
}

// viewer panel: media/text preview of the active path from selected_paths
// (doc "viewer panel"). Files are streamed by the GUI server's
// /fsraw endpoint (HTTP Range supported, so audio/video can seek).
// When the active path is a directory, its contents are shown as a
// thumbnail grid the user can click into (drill-in, with a back button).

import { byId, el, looksLikeText, messageOf, thumbnail } from '/__ui.js';
import { createPagedList } from '/__paged-list.js';
import { fileMenuItems, parentDir } from '/__file-actions.js';
import {
  SAVE_INTERVAL_MS,
  MIN_DELTA,
  SEEK_STEP,
  SEEK_STEP_LONG,
  VOLUME_STEP,
  playbackAction,
  resumeTarget,
  formatPosition,
  seekTarget,
  nextSpeed,
  nextVolume,
  loadPosition,
  savePosition,
  clearPosition,
} from './playback.js';
import { fetchMounts, offlineMountFor, relativeTo, unavailableLabel } from '/__mounts.js';
import {
  GAP_SECONDS,
  GIF_SECONDS,
  IMAGE_SECONDS,
  PLAY_OPTIONS,
  createPlaylist,
  gifDurationMs,
  parsePlayArgs,
  stepFor,
} from './slideshow.js';

// Directory grid: render this many thumbnails per window (more on scroll), so
// opening a folder with thousands of files does not build/load them all at
// once. The size comes from the GUI config (`[page-size].file`); this is only
// the fallback when no config value is provided.
const DIR_PAGE_DEFAULT = 150;

// What a file is shown as comes from its content, not its name
// (doc "GUI file endpoints"): the GUI reads the kind from the file's first
// bytes (`fs.stat().kind`) — image, gif, video, audio or document. Text has no
// magic bytes, so it is what is left, decided below.
//
// The viewer limits are *fallbacks*: the effective values come from the GUI
// config (`[panel-defaults.viewer]`), read in `mount`.

// Known text extensions: a *fast path* to the text preview (not the only way
// in). A file with one of these is always rendered as text — this is also how
// encodings the content sniff rejects still preview (UTF-16 holds NULs). Any
// other extension (including none) is decided by `looksLikeText` on its bytes,
// so `.tid`/`.cmake`/an extensionless script need not be listed here.
const TEXT = new Set([
  'txt', 'md', 'org', 'json', 'toml', 'yaml', 'yml', 'xml', 'html', 'css', 'js', 'ts',
  'rs', 'py', 'sh', 'c', 'h', 'cpp', 'java', 'log', 'csv', 'ini', 'conf',
]);
const TEXT_PREVIEW_LIMIT = 256 * 1024;
// A document is rendered page by page by the GUI server (`GET /document`),
// never handed to the WebView as a file: see `server/document.rs` for why.
// Resolution of a rendered page, in DPI. 150 is a readable A4 page on a normal
// screen; the zoom controls work on the resulting image, so this is the
// *rendering* detail, not the displayed size.
const DOCUMENT_DPI = 150;

// Zoom: a step multiplies the current scale; bounds keep the manual size sane.
const ZOOM_STEP = 1.25;
const ZOOM_MIN = 0.05;
const ZOOM_MAX = 40;

// What the media poster says, before the volume note it may carry.
const POSTER_HINT = '\u25b6 play (space) \u2014 the player loads on demand';

/** A configured extension list as a lowercase Set, or `fallback` when the
 *  config has none (or something that is not a list of strings).
 *  @param {unknown} configured @param {Set<string>} fallback */
function extensionSet(configured, fallback) {
  if (!Array.isArray(configured)) return fallback;
  const extensions = configured.filter((e) => typeof e === 'string');
  return extensions.length ? new Set(extensions.map((e) => e.toLowerCase())) : fallback;
}

/** A configured number, or `fallback` when it is missing or not a finite one.
 *  @param {unknown} configured @param {number} fallback */
function positiveNumber(configured, fallback) {
  return typeof configured === 'number' && Number.isFinite(configured) && configured > 0
    ? configured
    : fallback;
}

/** A configured number of seconds that may be 0, or `fallback`.
 *  @param {unknown} configured @param {number} fallback */
function nonNegativeNumber(configured, fallback) {
  return typeof configured === 'number' && Number.isFinite(configured) && configured >= 0
    ? configured
    : fallback;
}

/**
 * Where "up one level" lands (doc "viewer panel"): out of a drilled-in
 * listing, or — when the panel is following the selection — into the folder
 * holding the previewed file, so Backspace browses upwards the way it does in
 * the file manager rather than being a dead end.
 *
 * `move: false` = nowhere to go (nothing shown, or already at "/");
 * `path: null` = the walk reached the selected path, so follow the selection
 * again instead of holding a drill-in of our own.
 *
 * @param {string|null} local the drill-in path, or null while following
 * @param {string|undefined} selectedPath the followed selection's path
 * @returns {{move: false} | {move: true, path: string|null}}
 */
export function backTarget(local, selectedPath) {
  const current = local ?? selectedPath;
  if (!current) return { move: false };
  const parent = parentDir(current);
  if (parent === current) return { move: false }; // at the filesystem root
  return { move: true, path: parent === selectedPath ? null : parent };
}

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
  const { workspace, fs, commands, daemon, statusBar } = metafolder;
  const dirPage = metafolder.pageSize ?? DIR_PAGE_DEFAULT;
  const statusMessageMs = metafolder.settings?.statusMessageMs ?? 5000;
  // The text fast path and the viewer's limits (GUI config
  // `[panel-defaults.viewer]`, falling back to the module constants above).
  const { defaults } = metafolder;
  const text = extensionSet(defaults.textExtensions, TEXT);
  const documentDpi = positiveNumber(defaults.documentDpi, DOCUMENT_DPI);
  const textPreviewLimit = positiveNumber(defaults.textPreviewLimit, TEXT_PREVIEW_LIMIT);
  const zoomStep = positiveNumber(defaults.zoomStep, ZOOM_STEP);
  const zoomMin = positiveNumber(defaults.zoomMin, ZOOM_MIN);
  const zoomMax = positiveNumber(defaults.zoomMax, ZOOM_MAX);
  // The slideshow's waits (doc "Slideshow").
  const slideshowImageMs = positiveNumber(defaults.slideshowImageSeconds, IMAGE_SECONDS) * 1000;
  const slideshowGifMs = positiveNumber(defaults.slideshowGifSeconds, GIF_SECONDS) * 1000;
  const slideshowGapMs = nonNegativeNumber(defaults.slideshowGapSeconds, GAP_SECONDS) * 1000;

  /** @type {string[]} */
  let paths = [];
  let activeIndex = 0;
  // The metarecord the selection points at ({ uuid, repo }), published by
  // metarecord-list alongside selected_paths. It is what the playback position
  // is stored on — so it only applies while the viewer follows the selection
  // (a drill-in path has no metarecord in hand).
  /** @type {Selected|null} */
  let selected = null;
  // The workspace's repository, read with the selection: what the file actions
  // of the right-click menu need (the trash is a repository's), whether or not
  // the file shown has a metarecord.
  /** @type {string|null} */
  let activeRepo = null;
  /** @type {string|null} uuid last recorded in the recently-viewed list, so
   *  following the same selection again (or a re-render) does not churn the
   *  list. */
  let lastViewedUuid = null;
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
  const mediaWarning = byId(root, 'media-warning');
  const zoomLabel = byId(root, 'zoom-label');
  const resumeHint = byId(root, 'resume-hint');
  const docPager = byId(root, 'doc-pager');
  const pageLabel = byId(root, 'page-label');
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

  // Volume/mute are panel state, not the media element's. The element only
  // exists while something plays, so a level living on it could only ever be
  // set *after* the sound had already come out — and a video that starts at
  // full blast has already made the noise by the time you can turn it down.
  // Held here instead, they are settable in front of the poster, applied to
  // each element as it is built, and kept across files like the zoom below.
  let volume = 1;
  let muted = false;

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

  /**
   * The document being previewed, or null when the view holds anything else.
   * `img` is kept so turning a page is an image swap rather than a re-render:
   * the element stays the zoom target, so a zoom the user set survives the
   * page turn (and the viewer does not flash).
   *
   * @type {{path: string, page: number, pages: number, img: HTMLImageElement}|null}
   */
  let document_ = null;

  /** @param {string} path @param {number} page */
  function documentUrl(path, page) {
    const auth = metafolder.sessionToken
      ? `&token=${encodeURIComponent(metafolder.sessionToken)}`
      : '';
    return `${metafolder.guiServer}/document?path=${encodeURIComponent(path)}` +
      `&page=${page}&dpi=${documentDpi}${auth}`;
  }

  /** @param {string} text */
  function placeholder(text) {
    clearZoomTarget();
    viewer.replaceChildren(el('p', { class: 'placeholder' }, text));
  }

  // A playback-quality warning shown above the viewer, or null to clear it.
  // Unlike `placeholder`, this does not replace the preview: the file does
  // play, it just plays badly, and hiding it would be worse than the warning.
  /** @param {string|null} text */
  function setMediaWarning(text) {
    mediaWarning.textContent = text ?? '';
    mediaWarning.hidden = text === null;
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
    zoomFactor = Math.min(zoomMax, Math.max(zoomMin, zoomFactor * mult));
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
  // The media file shown as a poster, waiting for playback to be asked for
  // before its element is built (see `renderMedia`), or null when none is (or
  // when it has already been mounted).
  /** @type {{kind: 'audio'|'video', path: string, url: string, generation: number}|null} */
  let pendingMedia = null;

  // --- Playback position ------------------------------------------------
  //
  // Where the user stopped watching, kept on the metarecord of the played
  // file so the next open resumes there (doc "viewer panel"). The
  // state of the video currently mounted, or null when none is (or when it
  // has no metarecord to store a position on): { media, repo, uuid, stored,
  // timer }, with `stored` mirroring the field's value so a redundant write
  // can be skipped.
  /** @type {Playback|null} */
  let playback = null;
  // A write in flight: pause/seek/interval can all fire while one is running,
  // and each write is an event-log write — never stack them.
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

  // Where the video about to be shown was left off, or null when there is
  // nothing to resume (or no metarecord to have stored one). Read *before* the
  // element is built: doing it afterwards left a daemon round trip during which
  // the browser had already loaded and painted the opening frame, only for the
  // answer to jump it elsewhere — the flicker on every resumed video.
  /** @returns {Promise<number|null>} */
  async function loadResume() {
    if (localPath !== null || selected === null) return null; // no metarecord to store it on
    return loadPosition(daemon, selected.repo, selected.uuid);
  }

  // Arm position tracking on a <video> that has not been given its source yet:
  // seek to the stored position, then keep it up to date. Writes happen on
  // pause/seek/end, on teardown, and periodically while playing — never on
  // `timeupdate` (which fires several times a second, and every write is an
  // event-log write).
  /** @param {HTMLMediaElement} media @param {number|null} stored
   *  @param {boolean} [seek] false: start from the beginning (a slideshow),
   *  though the stored position is still known — and so cleared when the
   *  video is watched to its end */
  function attachPlayback(media, stored, seek = true) {
    if (localPath !== null || selected === null) return; // no metarecord to store it on
    const { uuid, repo } = selected;
    /** @type {Playback} */
    const state = { media, repo, uuid, stored, timer: null };
    playback = state;

    if (stored !== null && seek) {
      // The element has no source yet, so `loadedmetadata` is still ahead of
      // us: the seek lands the moment the duration is known, before a single
      // frame has been decoded — no opening frame is ever shown.
      media.addEventListener(
        'loadedmetadata',
        () => {
          const target = resumeTarget(stored, media.duration);
          if (target === null) return;
          media.currentTime = target;
          showResumeHint(target);
        },
        { once: true },
      );
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
    pendingMedia = null;
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

  // Up one level: out of the drill-in, or into the folder holding the followed
  // file — returning to following the selection once we reach the originally
  // selected path.
  function navigateBack() {
    const target = backTarget(localPath, paths[activeIndex]);
    if (!target.move) return;
    localPath = target.path;
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
  // the full decode pipeline), and whether the decoder it would pick is too
  // slow for the stream. Returns null when the probe is unreachable.
  /**
   * @param {string} path
   * @returns {Promise<{missing: string[], slow: string|null}|null>}
   */
  async function probeFile(path) {
    try {
      const auth = metafolder.sessionToken
        ? `&token=${encodeURIComponent(metafolder.sessionToken)}`
        : '';
      const response = await fetch(
        `${metafolder.guiServer}/__media-probe?path=${encodeURIComponent(path)}${auth}`,
      );
      if (response.ok)
        return /** @type {{missing: string[], slow: string|null}} */ (await response.json());
    } catch {
      // Unreachable: caller decides how to proceed.
    }
    return null;
  }

  // A media file is NEVER mounted on sight. Creating an <audio>/<video> makes
  // WebKit's web process build a GStreamer pipeline, and a pipeline that stalls
  // — a software decoder on a machine with no GPU, a 4K AV1 stream — takes the
  // whole window with it: no repaint, no keys, nothing to do but kill it. An
  // interactive walk that shows one file per question (the tagging scripts) hits
  // that reliably. So a preview shows the poster frame instead — rendered by
  // ffmpeg out of process and cached — and the element is built only once
  // playback is actually asked for. Same bargain as the GIF stills above.
  // Show the poster standing in for a media file: its first frame for a video,
  // the type glyph for audio (nothing to extract a frame from). Clicking it —
  // like `viewer:play-pause` — is what loads the player.
  /**
   * @param {'audio'|'video'} kind @param {string} path @param {string} url
   * @param {number} generation
   */
  function renderMedia(kind, path, url, generation) {
    pendingMedia = { kind, path, url, generation };
    viewer.replaceChildren(
      el(
        'button',
        {
          class: 'media-poster',
          title: 'Load the player and play this file',
          onclick: () => void mountMedia(true),
        },
        thumbnail(metafolder.guiServer, path, {
          glyphClass: 'glyph',
          token: metafolder.sessionToken,
        }),
        el('span', { class: 'hint' }, POSTER_HINT + volumeSuffix()),
      ),
    );
  }

  // Build the element for the media waiting behind its poster, and start it when
  // asked. Everything the old eager path did happens here instead, once, at the
  // user's request: the decoder probe (gst-discoverer, which reports the missing
  // plugins without ever building the pipeline that crashes WebKit), the stored
  // playback position, the zoom target.
  /** @param {boolean} play @param {{resume?: boolean}} [options] `resume:
   *  false` starts from the beginning (a slideshow)
   *  @returns {Promise<HTMLMediaElement|null>} the element, or null when none
   *  was built (nothing waiting, the view moved on, the file cannot play) */
  async function mountMedia(play, { resume = true } = {}) {
    if (!pendingMedia) return null;
    const { kind, path, url, generation } = pendingMedia;
    const current = () => generation === renderGeneration;
    const support = await mediaSupport();
    if (!current()) return null;
    if (!support[kind]) {
      placeholder(
        `media preview disabled: missing GStreamer elements: ` +
          `${support.missing.join(', ')} (install gst-plugins-good)`,
      );
      return null;
    }
    placeholder('checking media support…');
    const probe = await probeFile(path);
    if (!current()) return null;
    if (probe && probe.missing.length > 0) {
      placeholder(
        `cannot play this file: missing GStreamer plugin(s): ${probe.missing.join(', ')} ` +
          `(try gst-libav / gst-plugins-bad / gst-plugins-ugly)`,
      );
      return null;
    }
    // Every decoder is present, but the one GStreamer will pick cannot keep
    // up with this stream: play it anyway (the user may only want to seek
    // through it) and say why it stutters, so a slow decoder is not mistaken
    // for a broken file — the symptom that motivated this check.
    if (probe && probe.slow) setMediaWarning(probe.slow);
    // Read the resume position before building the element, so that everything
    // the first frame depends on is already known when loading starts.
    const stored = kind === 'video' ? await loadResume() : null;
    if (!current()) return null; // the view moved on while we read
    // `preload="metadata"`: fetch only enough for the first frame and duration
    // — never buffer/decode the whole stream ahead of the viewer.
    const media = el(kind, { controls: true, preload: 'metadata' });
    activeMedia = media;
    pendingMedia = null; // mounted: play/pause is a transport control again
    // The probe found nothing missing, but keep a light fallback for other
    // runtime failures (a corrupt stream, an unreadable file).
    media.addEventListener('error', () => {
      if (current()) placeholder('cannot play this file (unsupported or corrupt media)');
    });
    // The chosen level applies from the very first sample: set before the
    // source, so nothing is ever heard at a volume the user did not ask for.
    media.volume = volume;
    media.muted = muted;
    viewer.replaceChildren(media);
    // Only video is zoomable; audio has no visual frame. (Narrowing `kind` does
    // not narrow `media`, hence the instanceof.)
    if (media instanceof HTMLVideoElement) {
      setZoomTarget(media);
      attachPlayback(media, stored, resume);
    }
    // The source goes on last, once every listener — the resume seek included —
    // is armed: nothing can load, and therefore be shown, before them.
    media.src = url;
    if (play) void media.play().catch(() => {});
    return media;
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

  // Text preview. Fetches only the first `textPreviewLimit` bytes (the GUI
  // server honours the Range header — 206 Partial Content — so even a multi-GB
  // log only pulls this capped prefix into memory), then either renders it as a
  // <pre> or, when `force` is false and the bytes look binary, gives up.
  //
  // `force` distinguishes the two callers: an explicit text extension (.txt,
  // .md, …) is rendered as text unconditionally — that path also covers text
  // encodings the content sniff cannot (UTF-16 carries NULs) — whereas an
  // unknown/absent extension is only shown when `looksLikeText` accepts the
  // prefix, so `.tid`/`.cmake`/an extensionless script preview while a real
  // binary falls through to "no preview" instead of a wall of mojibake.
  //
  // We read arrayBuffer (not .text()) so the NUL sniff sees raw bytes and so
  // the decode is non-fatal: stray/latin-1 bytes become replacement chars
  // rather than throwing.
  /** @param {string} url @param {number} generation @param {boolean} force */
  async function renderText(url, generation, force) {
    const current = () => generation === renderGeneration;
    try {
      const response = await fetch(url, {
        headers: { range: `bytes=0-${textPreviewLimit - 1}` },
      });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const bytes = new Uint8Array(await response.arrayBuffer());
      if (!current()) return;
      if (!force && !looksLikeText(bytes)) {
        placeholder('no preview available for this format');
        return;
      }
      const text = new TextDecoder().decode(bytes);
      const pre = el('pre', {}, text);
      viewer.replaceChildren(pre);
      if (response.status === 206 && bytes.length >= textPreviewLimit) {
        pre.textContent += '\n… (truncated preview)';
      }
    } catch (error) {
      if (current()) placeholder(`cannot load the file: ${messageOf(error)}`);
    }
  }

  // Document preview: the pages are PNGs rendered out of process by poppler
  // (`GET /document`), because the file's own bytes must never be loaded as a
  // document in this origin — `/fsraw` carries the session token in its URL
  // (see `server/fsraw.rs` and the panel-invariants test). The page count comes
  // first, so navigation knows where the document ends.
  /** @param {string} path @param {number} generation */
  async function renderDocument(path, generation) {
    const auth = metafolder.sessionToken
      ? `&token=${encodeURIComponent(metafolder.sessionToken)}`
      : '';
    let pages;
    try {
      const response = await fetch(
        `${metafolder.guiServer}/document/info?path=${encodeURIComponent(path)}${auth}`,
      );
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      pages = Number((await response.json()).pages) || 0;
    } catch {
      pages = 0;
    }
    if (generation !== renderGeneration) return;
    if (pages < 1) {
      // No poppler installed, a file outside every loaded repository, or one
      // poppler cannot read — the same outcome for the viewer either way.
      placeholder('no preview available for this format');
      return;
    }
    const img = el('img', { onerror: () => placeholder('cannot render this page') });
    img.src = documentUrl(path, 1);
    viewer.replaceChildren(img);
    document_ = { path, page: 1, pages, img: /** @type {HTMLImageElement} */ (img) };
    updateDocPager();
    setZoomTarget(/** @type {HTMLImageElement} */ (img));
  }

  function updateDocPager() {
    docPager.hidden = document_ === null;
    if (document_) pageLabel.textContent = `${document_.page} / ${document_.pages}`;
  }

  // Moves to page `page`, clamped to the document. A no-op when no document is
  // shown, so the page commands are harmless bound in the panel at large.
  /** @param {number} page */
  function goToPage(page) {
    if (!document_) return;
    const next = Math.min(document_.pages, Math.max(1, page));
    if (next === document_.page) return;
    document_.page = next;
    document_.img.src = documentUrl(document_.path, next);
    updateDocPager();
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
    // The previous file's playback warning does not apply to the next one.
    setMediaWarning(null);
    // Any zoomable media from the previous view is gone; the image/video
    // branches below re-arm it. The GIF checkbox only applies to the GIF
    // branch, which un-hides it.
    clearZoomTarget();
    gifAnimateWrap.hidden = true;
    document_ = null;
    updateDocPager();
    revokeStaticGif();
    const path = viewedPath();
    if (!path) {
      placeholder('No file selected');
      slideStep(generation, 'skip');
      return;
    }
    // The file may be on a volume that is not plugged in. Saying "no preview
    // available" there would leave the user guessing why a file the database
    // knows about will not open (doc "Unmounted volumes in the GUI").
    const mount = await unavailableMount(path);
    if (generation !== renderGeneration) return;
    if (mount) {
      placeholder(unavailableLabel(mount));
      slideStep(generation, 'skip');
      return;
    }
    // A directory has no meaningful file preview: list its contents instead.
    /** @type {{is_dir?: boolean, kind?: string|null}|null} */
    let info = null;
    try {
      info = /** @type {{is_dir?: boolean, kind?: string|null}} */ (await fs.stat(path));
    } catch {
      // Unreachable/removed: fall through to the file preview.
    }
    if (info?.is_dir) {
      slideStep(generation, 'skip');
      await renderDirectory(path, generation);
      return;
    }
    const extension = (path.split('.').pop() ?? '').toLowerCase();
    const url = rawUrl(path);

    // The kind is the content's. A name that lies — or says nothing — changes
    // nothing: `main.ts` is text and `clip.ts` a video, and both are right.
    const kind = info?.kind ?? null;
    // What the slideshow, when one runs, does with it once it is shown.
    const step = stepFor(kind);

    if (kind === 'image' || kind === 'gif') {
      const gif = kind === 'gif';
      gifAnimateWrap.hidden = !gif;
      const img = el('img', { onerror: () => placeholder('cannot load the file') });
      // A slideshow shows a GIF moving: it waits for one pass of it.
      if (gif && !animateGifs && !slideshowFollowed()) {
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
      slideStep(generation, step, url);
    } else if (kind === 'audio' || kind === 'video') {
      renderMedia(kind, path, url, generation);
      slideStep(generation, step);
    } else if (kind === 'document') {
      slideStep(generation, step);
      await renderDocument(path, generation);
    } else if (text.has(extension)) {
      // A known text extension: render as text unconditionally.
      slideStep(generation, step);
      await renderText(url, generation, true);
    } else {
      slideStep(generation, step);
      // Neither a known kind nor a listed text extension: sniff the bytes and
      // preview it only if it looks like text (so plain-text files with an
      // unlisted extension work).
      await renderText(url, generation, false);
    }
  }

  /**
   * The offline mount point `path` sits on, or null — null as well when no
   * repository is active, or when the daemon cannot answer: an unavailable
   * volume is a nicer message, never a precondition for previewing a file.
   *
   * @param {string} path absolute path
   */
  async function unavailableMount(path) {
    try {
      const repo = selected?.repo ?? (await workspace.get('active_repo'));
      if (typeof repo !== 'string' || repo === '') return null;
      const mounts = await fetchMounts(daemon, repo);
      if (mounts.length === 0) return null;
      return offlineMountFor(mounts, relativeTo(await daemon.repoRoot(repo), path));
    } catch {
      return null;
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
    if (show) {
      // The selection moved: the slideshow's own step, or the user choosing a
      // file — then it goes on from that one (when it is in the list).
      const uuid = selected?.uuid ?? null;
      if (uuid !== null && uuid !== show.expected) show.list.jumpTo(uuid);
      show.expected = null;
      clearShowTimer();
    }
    const repo = await workspace.get('active_repo');
    activeRepo = typeof repo === 'string' && repo !== '' ? repo : null;
    // Record the view in the repo's recently-viewed list (GUI-side, no daemon
    // write) when the selection points at a tracked metarecord — only on an
    // actual change, so re-selecting the same file does not churn the list.
    if (selected?.uuid && selected.uuid !== lastViewedUuid) {
      lastViewedUuid = selected.uuid;
      void metafolder.recent.touch(selected.repo, selected.uuid);
    }
    metafolder.whenVisible(rerender);
  }

  // Zoom commands (also reachable from the command input / a keybinding) and
  // their toolbar buttons. The handlers no-op unless an image/video is shown.

  // Document paging. Like the zoom commands, each is a no-op unless a document
  // is being previewed.

  // --- Playback controls -------------------------------------------------
  //
  // Transport commands for the mounted <audio>/<video>
  // (doc "Media preview"): what a media player's keyboard does, reachable from the command
  // input and the keybindings. They are no-ops when the preview holds no media
  // (an image, a text file, a directory listing), so a key bound in this panel
  // is always harmless.

  /** @param {(media: HTMLMediaElement) => void} fn */
  function withMedia(fn) {
    if (activeMedia) fn(activeMedia);
  }

  void commands.register('viewer:play-pause', {
    label: 'Viewer: play / pause the media, or the animation of a GIF',
    handler: async () => {
      // A slideshow waiting (an image on screen, the pause after a video):
      // hold it, or let it go on.
      if (show && (show.timer !== null || show.paused)) {
        toggleShowPause();
        return;
      }
      // A GIF has no player: play/pause is its animation, the space key the
      // same toggle as the checkbox.
      if (!gifAnimateWrap.hidden) {
        setAnimateGifs(!animateGifs);
        return;
      }
      // Nothing mounted yet: the poster is showing, so this is the gesture that
      // loads the player rather than a transport control.
      if (pendingMedia) {
        await mountMedia(true);
        return;
      }
      withMedia((media) => {
        // play() rejects when the browser refuses to start (autoplay policy, a
        // decode failure): the media element reports that on its own.
        if (media.paused) void media.play().catch(() => {});
        else media.pause();
      });
    },
  });

  /** @param {number} delta seconds, negative to go back */
  function seekBy(delta) {
    withMedia((media) => {
      const target = seekTarget(media.currentTime, delta, media.duration);
      if (target !== null) media.currentTime = target;
    });
  }
  void commands.register('viewer:restart', {
    label: 'File: play the media from the start',
    handler: () => withMedia((media) => (media.currentTime = 0)),
  });

  /** @param {number} rate */
  function applySpeed(rate) {
    withMedia((media) => {
      media.playbackRate = rate;
      void statusBar.message(`Playback speed ${rate}×`, statusMessageMs);
    });
  }
  // ── Generic transport / view commands ───────────────────────────────────
  //
  // One command per family, the variant as its first argument, so a keybinding
  // names what it does (`viewer:zoom in`) and a user can bind a step the shipped
  // defaults never offered (`viewer:seek +5`).

  /** @type {Record<string, () => unknown>} */
  const ZOOMS = {
    in: () => zoomBy(zoomStep),
    out: () => zoomBy(1 / zoomStep),
    fit: () => zoomFit(),
    reset: () => zoomReset(),
  };
  /** @type {Record<string, () => unknown>} */
  const PAGES = {
    next: () => goToPage((document_?.page ?? 1) + 1),
    prev: () => goToPage((document_?.page ?? 1) - 1),
    first: () => goToPage(1),
    last: () => goToPage(document_?.pages ?? 1),
  };
  /** @type {Record<string, () => unknown>} */
  const SPEEDS = {
    up: () => withMedia((media) => applySpeed(nextSpeed(media.playbackRate, 1))),
    down: () => withMedia((media) => applySpeed(nextSpeed(media.playbackRate, -1))),
    reset: () => applySpeed(1),
  };
  /** @type {Record<string, () => unknown>} */
  const VOLUMES = {
    up: () => changeVolume(VOLUME_STEP),
    down: () => changeVolume(-VOLUME_STEP),
  };

  /** Builds the registration options for a one-of-these command. It returns
   *  the options rather than registering, so every command name stays a
   *  literal at its `commands.register` call — which is what a grep, and the
   *  test that checks no keybinding names a dead command, rely on.
   *  @param {string} noun @param {Record<string, () => unknown>} table
   *  @param {string} label */
  function variant(noun, table, label) {
    const names = Object.keys(table);
    return {
      label: `${label} (${names.join(' / ')})`,
      args: [
        {
          name: noun,
          prompt: () => `Which? (${names.join(' / ')})`,
          complete: () => names,
        },
      ],
      /** @param {MetafolderApi} _mf @param {string} which */
      handler: (_mf, which) => {
        const run = table[which];
        if (!run)
          throw new Error(`unknown ${noun}: "${which ?? ''}" (expected ${names.join(' / ')})`);
        return run();
      },
    };
  }

  void commands.register('viewer:zoom', variant('zoom', ZOOMS, 'File: zoom the previewed image/video'));
  void commands.register('viewer:page', variant('page', PAGES, 'File: go to a page of the document'));
  void commands.register('viewer:speed', variant('speed', SPEEDS, 'File: step the playback rate'));
  void commands.register('viewer:volume', variant('volume', VOLUMES, 'File: step the volume'));
  void commands.register('viewer:toggle-gif-animation', {
    label: 'File: play or stop the animation of a GIF',
    handler: () => setAnimateGifs(!animateGifs),
  });

  void commands.register('viewer:seek', {
    label: `File: seek the previewed media by a signed number of seconds (e.g. +${SEEK_STEP}, -${SEEK_STEP_LONG})`,
    args: [
      {
        name: 'seconds',
        prompt: () => 'Seek by how many seconds? (signed, e.g. +10 or -60)',
        complete: () => [`+${SEEK_STEP}`, `-${SEEK_STEP}`, `+${SEEK_STEP_LONG}`, `-${SEEK_STEP_LONG}`],
      },
    ],
    handler: (_mf, raw) => {
      const seconds = Number(raw);
      if (!Number.isFinite(seconds)) throw new Error(`not a number of seconds: "${raw ?? ''}"`);
      return seekBy(seconds);
    },
  });

  void commands.register('viewer:back', {
    label: 'File: go up one level (out of a listing, or to the containing folder)',
    handler: navigateBack,
  });

  /** Push the panel's level onto whatever is showing: the mounted element, or
   *  the poster's hint (which is how a pre-playback setting is visible at all). */
  function applyVolume() {
    if (activeMedia) {
      activeMedia.volume = volume;
      activeMedia.muted = muted;
    }
    const hint = viewer.querySelector('.media-poster .hint');
    if (hint) hint.textContent = POSTER_HINT + volumeSuffix();
  }

  /** What the poster adds about the volume — nothing at all when it is the
   *  plain, unmuted 100 % nobody needs telling about. */
  function volumeSuffix() {
    if (muted) return ' \u2014 muted';
    return volume < 1 ? ` \u2014 volume ${Math.round(volume * 100)}%` : '';
  }

  /** @param {number} delta */
  function changeVolume(delta) {
    volume = nextVolume(volume, delta);
    // Nudging the volume up is the natural way to undo a mute.
    if (delta > 0) muted = false;
    applyVolume();
    void statusBar.message(
      `Volume ${Math.round(volume * 100)}%${muted ? ' (muted)' : ''}`,
      statusMessageMs,
    );
  }
  void commands.register('viewer:mute', {
    label: 'File: mute / unmute',
    handler: () => {
      muted = !muted;
      applyVolume();
      void statusBar.message(
        muted ? 'Muted.' : `Unmuted (volume ${Math.round(volume * 100)}%).`,
        statusMessageMs,
      );
    },
  });

  /** @param {boolean} on */
  function setAnimateGifs(on) {
    animateGifs = on;
    gifAnimateBox.checked = on;
    void renderViewer();
  }
  gifAnimateBox.addEventListener('change', () => setAnimateGifs(gifAnimateBox.checked));

  // --- Slideshow ----------------------------------------------------------
  //
  // `viewer:play` (doc "Slideshow"): one file after another, taken from the
  // checked selection or the query the metarecord list shows. It moves the
  // workspace's selection — every panel follows, this one included — and each
  // file, once shown, says what comes next (`slideStep`): an audio or a video
  // plays to its end, then a short pause; an image stays a few seconds; a GIF
  // one pass of its animation; anything else is passed over.

  /**
   * @typedef {{
   *   repo: string,
   *   list: ReturnType<typeof createPlaylist>,
   *   expected: string|null,
   *   skipped: number,
   *   timer: ReturnType<typeof setTimeout>|null,
   *   deadline: number,
   *   remaining: number|null,
   *   then: (() => unknown)|null,
   *   paused: boolean,
   * }} Show
   * `expected`: the uuid the slideshow itself selected last, to tell its own
   * steps from the user's. `skipped`: files passed over in a row — a whole
   * list of them ends the slideshow rather than spinning on a loop.
   */
  /** @type {Show|null} */
  let show = null;

  /** Whether the file shown is the slideshow's (not a drill-in of the user's). */
  function slideshowFollowed() {
    return show !== null && localPath === null;
  }

  function clearShowTimer() {
    if (!show) return;
    if (show.timer !== null) clearTimeout(show.timer);
    show.timer = null;
    show.remaining = null;
    show.then = null;
  }

  /** Runs `then` in `ms`, unless the slideshow is paused: then it waits for
   *  the resume. @param {number} ms @param {() => unknown} then */
  function showAfter(ms, then) {
    const s = show;
    if (!s) return;
    clearShowTimer();
    s.then = then;
    if (s.paused) {
      s.remaining = ms;
      return;
    }
    s.deadline = Date.now() + ms;
    s.timer = setTimeout(() => {
      if (show !== s) return;
      s.timer = null;
      s.then = null;
      void then();
    }, ms);
  }

  function toggleShowPause() {
    const s = show;
    if (!s) return;
    if (s.paused) {
      s.paused = false;
      const { remaining, then } = s;
      if (then) showAfter(remaining ?? 0, then);
      void statusBar.message('Slideshow resumed.', statusMessageMs);
      return;
    }
    const then = s.then;
    const remaining = s.timer !== null ? Math.max(0, s.deadline - Date.now()) : s.remaining;
    if (s.timer !== null) clearTimeout(s.timer);
    s.timer = null;
    s.paused = true;
    s.then = then;
    s.remaining = remaining;
    void statusBar.message('Slideshow paused.', statusMessageMs);
  }

  /** @param {string|null} message */
  function stopShow(message) {
    clearShowTimer();
    show = null;
    if (message) void statusBar.message(message, statusMessageMs);
  }

  /** Counts a file passed over; true when the slideshow ended for it — every
   *  file of the list in a row, so there is nothing to play. @param {Show} s */
  function noteSkip(s) {
    s.skipped += 1;
    if (s.skipped < s.list.size) return false;
    stopShow('Slideshow: nothing to play.');
    return true;
  }

  /** The absolute paths of a metarecord's file. @param {string} repo
   *  @param {string} uuid @returns {Promise<string[]>} */
  async function filePaths(repo, uuid) {
    try {
      const relative = (await daemon.treePaths(repo, 'mfr_path', [uuid]))[uuid] ?? [];
      const rootPath = await daemon.repoRoot(repo);
      // Resolved paths are "/"-rooted, '' being the root itself.
      return relative.map((p) => (p === '' ? rootPath : `${rootPath}${p}`));
    } catch {
      return [];
    }
  }

  /** The content's kind of a file, null for a folder or what cannot be read.
   *  @param {string} path */
  async function kindOf(path) {
    try {
      const info = /** @type {{is_dir?: boolean, kind?: string|null}} */ (await fs.stat(path));
      return info?.is_dir ? null : (info?.kind ?? null);
    } catch {
      return null;
    }
  }

  /** Selects the next file of the list the slideshow can show, passing over
   *  the others — through `mf`: the `viewer:play` run for the first file (doc
   *  "Command transactions"), the panel's own API for the next ones, which
   *  its timers select once the run is over.
   *  @param {MetafolderApi} [mf] */
  async function advance(mf = metafolder) {
    const s = show;
    if (!s) return;
    clearShowTimer();
    for (;;) {
      const uuid = s.list.next();
      if (uuid === null) {
        stopShow('Slideshow finished.');
        return;
      }
      const found = await filePaths(s.repo, uuid);
      // What would be passed over is never selected: it would flash by, and
      // count as viewed (the recent panel) without having been.
      const kind = found.length > 0 ? await kindOf(found[0]) : null;
      if (show !== s) return; // stopped, or another started, meanwhile
      if (stepFor(kind) === 'skip') {
        if (noteSkip(s)) return;
        continue;
      }
      s.expected = uuid;
      await mf.workspace.set('selected_metarecord', { uuid, repo: s.repo });
      await mf.workspace.set('selected_paths', found);
      return;
    }
  }

  /**
   * What the slideshow does with the file `renderViewer` just showed.
   * @param {number} generation the render that showed it
   * @param {'media'|'image'|'gif'|'skip'} step
   * @param {string} [url] the file's raw URL (a GIF's bytes are read for its length)
   */
  function slideStep(generation, step, url) {
    const s = show;
    if (!s || generation !== renderGeneration) return;
    clearShowTimer();
    // The user walked off the slideshow's file (into a folder of the grid, up
    // out of the file): it holds until the selection moves again.
    if (!slideshowFollowed()) return;
    if (step === 'skip') {
      if (!noteSkip(s)) showAfter(0, advance);
      return;
    }
    s.skipped = 0;
    if (step === 'image') showAfter(slideshowImageMs, advance);
    else if (step === 'gif') void waitForGif(s, generation, url ?? '');
    else void playToEnd(s, generation);
  }

  /** One pass of the GIF shown, or the fixed wait when its length cannot be
   *  read. @param {Show} s @param {number} generation @param {string} url */
  async function waitForGif(s, generation, url) {
    /** @type {number|null} */
    let duration = null;
    try {
      const response = await fetch(url);
      if (response.ok) duration = gifDurationMs(new Uint8Array(await response.arrayBuffer()));
    } catch {
      // Unreadable: the fixed wait.
    }
    if (show !== s || generation !== renderGeneration) return;
    showAfter(duration ?? slideshowGifMs, advance);
  }

  /** Plays the media shown from its start, and goes on once it has ended (or
   *  could not play). @param {Show} s @param {number} generation */
  async function playToEnd(s, generation) {
    const media = await mountMedia(true, { resume: false });
    if (show !== s || generation !== renderGeneration) return;
    const next = () => {
      if (show === s && activeMedia === media) showAfter(slideshowGapMs, advance);
    };
    if (!media) {
      // It cannot play here (a missing decoder): its message stays up for the
      // pause, then the next file.
      if (!noteSkip(s)) showAfter(slideshowGapMs, advance);
      return;
    }
    media.addEventListener('ended', next);
    media.addEventListener('error', next);
  }

  /** @param {MetafolderApi} mf the `viewer:play` run
   *  @param {{target: 'selection'|'query', shuffle: boolean, loop: boolean}} request */
  async function startShow(mf, { target, shuffle, loop }) {
    stopShow(null);
    const repo = await mf.workspace.get('active_repo');
    if (typeof repo !== 'string' || repo === '') throw new Error('no active repository');
    /** @type {string[]} */
    let uuids;
    if (target === 'selection') {
      const checked = await mf.workspace.get('selected_metarecords');
      uuids = Array.isArray(checked) ? checked.filter((u) => typeof u === 'string') : [];
      if (uuids.length === 0) {
        void statusBar.message('No metarecords are checked — nothing to play.', statusMessageMs);
        return;
      }
    } else {
      // What the metarecord list shows, in its order (both published by it).
      const query = await mf.workspace.get('metarecord-list:effective-query');
      if (!query) {
        void statusBar.message('The metarecord list shows no query — nothing to play.', statusMessageMs);
        return;
      }
      const sort = await mf.workspace.get('metarecord-list:sort');
      const result = await daemon.call('POST', `/repos/${repo}/query`, {
        query,
        ...(Array.isArray(sort) && sort.length > 0 && { sort }),
      });
      uuids = Array.isArray(result) ? result.filter((u) => typeof u === 'string') : [];
      if (uuids.length === 0) {
        void statusBar.message('No metarecords match — nothing to play.', statusMessageMs);
        return;
      }
    }
    show = {
      repo,
      list: createPlaylist(uuids, { shuffle, loop }),
      expected: null,
      skipped: 0,
      timer: null,
      deadline: 0,
      remaining: null,
      then: null,
      paused: false,
    };
    const how = [shuffle && 'shuffled', loop && 'looping'].filter(Boolean).join(', ');
    void statusBar.message(
      `Slideshow: ${uuids.length} metarecord${uuids.length === 1 ? '' : 's'}${how ? ` (${how})` : ''}.`,
      statusMessageMs,
    );
    await advance(mf);
  }

  /** The option slots of `viewer:play`: either option, in either. */
  const playOption = {
    name: 'option',
    optional: true,
    prompt: () => `Option? (${PLAY_OPTIONS.join(' / ')})`,
    complete: () => PLAY_OPTIONS,
    /** @param {string[]} prior */
    when: (prior) => prior[0] !== 'stop',
  };
  void commands.register('viewer:play', {
    label: 'File: play one file after another — the checked selection or the query shown (selection / query / stop) [shuffle] [loop]',
    args: [
      {
        name: 'target',
        prompt: () => 'Play what? (selection / query / stop)',
        complete: () => ['selection', 'query', 'stop'],
      },
      playOption,
      playOption,
    ],
    /** @param {MetafolderApi} mf @param {...(string|undefined)} args */
    handler: (mf, ...args) => {
      const request = parsePlayArgs(args);
      const { target, shuffle, loop } = request;
      if (target !== 'stop') return startShow(mf, { target, shuffle, loop });
      if (show) stopShow('Slideshow stopped.');
      else void statusBar.message('No slideshow is running.', statusMessageMs);
      return undefined;
    },
  });

  // Keybindings for this panel live in keybindings.toml (when = "viewer").

  // Right-click the preview to cut/copy/paste/rename/duplicate/trash the shown
  // file (shared with the file manager — see /__file-actions.js). The *shown*
  // one: after a drill-in or a step back that is not the selected path, and a
  // menu aimed at the selection would act on something that is not on screen.
  metafolder.contextMenu.addDefaultItems(() => {
    // An untracked file has the same actions: they work on the path, and only
    // need a repository for its trash.
    const repo = selected?.repo ?? activeRepo;
    const path = viewedPath();
    if (!repo || typeof path !== 'string' || path === '') return [];
    return fileMenuItems({
      metafolder,
      repo,
      path,
      onChanged: () => void workspace.set('metarecords:dirty', Date.now()),
    });
  });

  byId(root, 'zoom-in').addEventListener('click', () => zoomBy(zoomStep));
  byId(root, 'zoom-out').addEventListener('click', () => zoomBy(1 / zoomStep));
  byId(root, 'zoom-fit').addEventListener('click', zoomFit);
  byId(root, 'zoom-reset').addEventListener('click', zoomReset);
  byId(root, 'page-prev').addEventListener('click', () => goToPage((document_?.page ?? 1) - 1));
  byId(root, 'page-next').addEventListener('click', () => goToPage((document_?.page ?? 1) + 1));

  // Read again on `view:refresh` (doc "Generic panel commands"): the file as it
  // is on disk now, or the folder's listing. A medium goes back behind its
  // poster, its position kept for the next play — as when the selection moves.
  metafolder.provide({ refresh: () => metafolder.whenVisible(rerender) });
  // The file shown, for the `file:` commands and the clipboard (doc "File
  // commands"): the workspace's selection, which this panel follows.
  metafolder.provide({ paths: (mf) => selectedPathsOf(mf) });

  workspace.onChange('selected_paths', (value) => void update(value));
  await update(await workspace.get('selected_paths'));

  // Release the media pipeline if the panel is unmounted (workspace closed or
  // panel type switched) so it cannot keep decoding in the background.
  return () => {
    stopShow(null);
    teardownMedia();
    revokeStaticGif();
    if (detachDirScroll) detachDirScroll();
  };
}

/** The message of a thrown error (the fs/daemon seams throw Error). */

/** The workspace's selected paths, as the `paths` a panel provides.
 *  @param {MetafolderApi} mf @returns {Promise<string[]>} */
async function selectedPathsOf(mf) {
  const value = await mf.workspace.get('selected_paths');
  return Array.isArray(value) ? value.filter((path) => typeof path === 'string') : [];
}

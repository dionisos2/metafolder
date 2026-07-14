// metafolder panel UI helpers — served at /__ui.js for panel types
// (spec-gui "The metafolder API"). Plain DOM building (no innerHTML:
// values come from user files, keep textContent semantics) and the
// canonical display form of the data model's Value variants.

/**
 * Element properties. Deliberately loose: `on*` handlers keep implicitly-`any`
 * parameters, which is what lets a panel write `onclick: () => select(i)` and
 * `onkeydown: (e) => …` without annotating either. The one cast this forces
 * (assigning through an index signature) is confined to `el` itself.
 *
 * @typedef {{class?: string | (string|false|null|undefined)[]} & Record<string, any>} ElProps
 */

/**
 * Anything `el` accepts as a child. Nested arrays are flattened (to any depth),
 * and null/undefined/false are dropped — so `cond && el(...)` works inline.
 * The array arm is `any[]` because a JSDoc typedef cannot reference itself.
 *
 * @typedef {Node|string|number|false|null|undefined|any[]} ElChild
 */

/**
 * Builds a DOM element: el('td', { class: ['cell', active && 'cursor'],
 * onclick: handler }, children...).
 *
 * Props: `class` is a string or an array (falsy entries dropped); `on*`
 * keys attach event listeners; existing IDL properties (value, checked,
 * colSpan, hidden, ...) are assigned, anything else becomes an attribute.
 * Children: nested arrays are flattened; null/undefined/false are
 * skipped; strings become text nodes.
 *
 * Generic over the tag, so `el('input', …)` is an `HTMLInputElement` at the
 * call site with nothing to annotate.
 *
 * @template {keyof HTMLElementTagNameMap} K
 * @param {K} tag
 * @param {ElProps} [props]
 * @param {...ElChild} children
 * @returns {HTMLElementTagNameMap[K]}
 */
export function el(tag, props = {}, ...children) {
  const element = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === 'class') {
      element.className = Array.isArray(value) ? value.filter(Boolean).join(' ') : value;
    } else if (key.startsWith('on')) {
      element.addEventListener(key.slice(2).toLowerCase(), value);
    } else if (key in element) {
      /** @type {any} */ (element)[key] = value;
    } else {
      element.setAttribute(key, value);
    }
  }
  element.append(
    ...children.flat(Infinity).filter((c) => c !== null && c !== undefined && c !== false),
  );
  return element;
}

/**
 * The element with `id`, or a throw.
 *
 * A panel owns its markup (its index.html), so a missing id is a bug in the
 * panel, not a runtime condition worth branching on. Throwing collapses ~140
 * `HTMLElement | null` types into one loud failure at mount that names the id,
 * instead of a null-deref surfacing three functions away.
 *
 * Pass the expected constructor to narrow — and assert — the element type:
 *
 *     const list = byId(root, 'entries');                   // HTMLElement
 *     const query = byId(root, 'query', HTMLInputElement);  // HTMLInputElement
 *     query.value = '';
 *
 * @template {abstract new (...args: any) => HTMLElement} [T=typeof HTMLElement]
 * @param {DocumentFragment|Document} root the panel's Shadow root
 * @param {string} id
 * @param {T} [type]
 * @returns {InstanceType<T>}
 */
export function byId(root, id, type) {
  const found = root.getElementById(id);
  if (!found) throw new Error(`panel markup has no #${id}`);
  const { nodeName } = found; // read before the instanceof: the negative branch narrows to never
  if (type && !(found instanceof type)) {
    throw new Error(`#${id} is a ${nodeName}, expected ${type.name}`);
  }
  return /** @type {InstanceType<T>} */ (found);
}

/**
 * The first element matching `selector`, or a throw. As {@link byId}.
 *
 * @template {abstract new (...args: any) => HTMLElement} [T=typeof HTMLElement]
 * @param {ParentNode} root
 * @param {string} selector
 * @param {T} [type]
 * @returns {InstanceType<T>}
 */
export function qs(root, selector, type) {
  const found = root.querySelector(selector);
  if (!found) throw new Error(`panel markup has nothing matching ${selector}`);
  const { nodeName } = found;
  if (type && !(found instanceof type)) {
    throw new Error(`${selector} is a ${nodeName}, expected ${type.name}`);
  }
  return /** @type {InstanceType<T>} */ (found);
}

/**
 * Every match for `selector`, as an array — so `.map`/`.filter` work directly,
 * and an empty result is an empty array, never null. Unlike {@link qs}, matching
 * nothing is a legitimate outcome (a list may be empty), so this never throws.
 *
 * @param {ParentNode} root
 * @param {string} selector
 * @returns {HTMLElement[]}
 */
export function qsa(root, selector) {
  return /** @type {HTMLElement[]} */ ([...root.querySelectorAll(selector)]);
}

/**
 * File extensions safe to display as an <img> thumbnail. Anything else must
 * NOT be handed to an <img>: pointing one at a video/pdf makes WebKit fetch
 * the whole file and try to decode it as an image, which balloons the web
 * process to gigabytes and crashes it.
 */
export const THUMBNAILABLE = new Set([
  'png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'avif', 'ico',
]);

/**
 * Whether a path/filename has an image extension safe for an <img> thumbnail.
 * @param {string} pathOrName
 */
export function isThumbnailable(pathOrName) {
  return THUMBNAILABLE.has(extensionOf(pathOrName));
}

/**
 * Video extensions for which the GUI server can extract a poster frame
 * (`GET /thumbnail`). Mirrors `VIDEO_EXTENSIONS` in `src/thumbnails.rs`.
 * A video must NEVER be handed to `<img src=/fsraw>` (WebKit would decode the
 * whole file and crash); the poster PNG from /thumbnail is safe.
 */
export const VIDEO_THUMBNAILABLE = new Set([
  'mp4', 'webm', 'mkv', 'mov', 'avi', 'wmv', 'm4v', 'mpg', 'mpeg', 'flv', '3gp', 'ts', 'm2ts',
]);

/**
 * Whether a path/filename is a video the server can make a poster for.
 * @param {string} pathOrName
 */
export function isVideoThumbnailable(pathOrName) {
  return VIDEO_THUMBNAILABLE.has(extensionOf(pathOrName));
}

/** @param {string} pathOrName */
function extensionOf(pathOrName) {
  return (pathOrName.split('.').pop() ?? '').toLowerCase();
}

// Common file types → an emoji icon, so a panel can tell music from a PDF from
// a video at a glance even where no real thumbnail is shown (the file-manager
// list, and the fallback for grid tiles that are not image/video). Built from
// extension groups into a flat `ext -> glyph` map.
const FILE_TYPE_GLYPHS = (() => {
  const groups = [
    [[...VIDEO_THUMBNAILABLE], '🎬'],
    [[...THUMBNAILABLE, 'tiff', 'tif', 'heic', 'heif'], '🖼️'],
    [['mp3', 'ogg', 'oga', 'flac', 'wav', 'm4a', 'opus', 'wma', 'aac', 'mid', 'midi'], '🎵'],
    [['pdf'], '📕'],
    [['doc', 'docx', 'odt', 'rtf'], '📝'],
    [['xls', 'xlsx', 'ods', 'csv'], '📊'],
    [['ppt', 'pptx', 'odp'], '📽️'],
    [['zip', 'tar', 'gz', 'tgz', 'bz2', 'xz', '7z', 'rar', 'zst', 'lz4'], '🗜️'],
  ];
  const map = new Map();
  for (const [extensions, glyph] of groups) {
    for (const extension of extensions) if (!map.has(extension)) map.set(extension, glyph);
  }
  return map;
})();

/**
 * Emoji icon for a file's type (🎬 video, 🎵 music, 📕 PDF, 🖼️ image,
 * 🗜️ archive…), or `fallback` (default 📄) for an unrecognised extension.
 *
 * @param {string} pathOrName
 * @param {string} [fallback]
 */
export function fileTypeGlyph(pathOrName, fallback = '📄') {
  return FILE_TYPE_GLYPHS.get(extensionOf(pathOrName)) ?? fallback;
}

/**
 * Builds a thumbnail node for a filesystem entry, shared by every panel that
 * shows files (file, metarecord-list) so the "never put a non-image in an
 * <img>" rule lives in one place. Image files become a lazy <img> at /fsraw;
 * videos and GIFs a lazy <img> at /thumbnail (a server-extracted poster frame
 * — never the raw video file, and a *still* first frame for a GIF, so a grid
 * of GIFs does not turn into a wall of animations). Both are bare <img>s, so
 * each panel's own `.thumb img` / `.card img` CSS styles them. Directories,
 * every other type, and a failed image/poster load fall back to a type glyph
 * <span> (see `fileTypeGlyph`) — except a GIF poster failure (ffmpeg missing,
 * file outside any repo), which retries the animated original at /fsraw
 * before the glyph: an animated thumbnail still beats no thumbnail.
 *
 * Options: `isDir`, `dirGlyph` (default 📁), `fileGlyph` (fallback for an
 * unknown type, default 📄), `glyphClass` (CSS class for the glyph span),
 * `token` (the session token appended as `?token=`, required for the protected
 * `/fsraw` and `/thumbnail` routes — spec-auth).
 *
 * @param {string} guiServer
 * @param {string|null} path
 * @param {{isDir?: boolean, dirGlyph?: string, fileGlyph?: string,
 *          glyphClass?: string, token?: string}} [options]
 * @returns {HTMLElement}
 */
export function thumbnail(guiServer, path, options = {}) {
  const { isDir = false, dirGlyph = '📁', fileGlyph = '📄', glyphClass = '', token = '' } = options;
  /** @param {string} text */
  const glyph = (text) => el('span', glyphClass ? { class: glyphClass } : {}, text);
  if (isDir) return glyph(dirGlyph);
  if (!path) return glyph(fileGlyph);
  const gif = extensionOf(path) === 'gif';
  const image = isThumbnailable(path);
  if (image || isVideoThumbnailable(path)) {
    const endpoint = image && !gif ? 'fsraw' : 'thumbnail';
    const auth = token ? `&token=${encodeURIComponent(token)}` : '';
    const img = el('img', {
      loading: 'lazy',
      src: `${guiServer}/${endpoint}?path=${encodeURIComponent(path)}${auth}`,
    });
    const glyphFallback = () => img.replaceWith(glyph(fileTypeGlyph(path, fileGlyph)));
    img.onerror = !gif
      ? glyphFallback
      : () => {
          img.onerror = glyphFallback;
          img.src = `${guiServer}/fsraw?path=${encodeURIComponent(path)}${auth}`;
        };
    return img;
  }
  return glyph(fileTypeGlyph(path, fileGlyph));
}

/**
 * Display form of a Value ({type, value} JSON, spec-data-model). `value` is
 * absent for `nothing` (explicit absence), which renders as ∅.
 *
 * @param {Metafolder.Value} field
 */
export function formatValue(field) {
  // The switch narrows the Value union, so each arm sees the right `value`.
  switch (field.type) {
    case 'nothing':
      return '∅';
    case 'tree_ref':
      return `${field.value.parent ?? '(root)'} / ${field.value.name}`;
    case 'externalref':
      return `${field.value.repo} :: ${field.value.metarecord}`;
    default:
      return String(field.value);
  }
}

// Memoized `name -> field[]` index per metarecord object (built once, reused by
// every cell). The WeakMap is keyed by the metarecord, so entries vanish when a
// result set is dropped.
const byNameCache = new WeakMap();

/**
 * A `name -> field[]` map of a metarecord's fields (a multi-map), built once.
 *
 * @param {Metafolder.Metarecord} metarecord
 * @returns {Map<string, Metafolder.Field[]>}
 */
export function byName(metarecord) {
  let map = byNameCache.get(metarecord);
  if (!map) {
    map = new Map();
    for (const f of metarecord.fields ?? []) {
      const list = map.get(f.name);
      if (list) list.push(f);
      else map.set(f.name, [f]);
    }
    byNameCache.set(metarecord, map);
  }
  return map;
}

/**
 * First field of an metarecord with the given name (fields are a multi-map).
 *
 * @param {Metafolder.Metarecord} metarecord
 * @param {string} name
 */
export function field(metarecord, name) {
  return byName(metarecord).get(name)?.[0];
}

/**
 * Every field of an metarecord with the given name (multi-map rows, in order).
 *
 * @param {Metafolder.Metarecord} metarecord
 * @param {string} name
 */
export function fields(metarecord, name) {
  return byName(metarecord).get(name) ?? [];
}

/**
 * Like formatValue, but as a DOM node where metarecord references are
 * clickable: the uuid of ref/refbase, the parent of a tree_ref, the
 * metarecord of an externalref. Clicking calls onOpen(uuid, repo) — repo is
 * null except for externalref (the referenced metarecord's repository).
 *
 * @param {Metafolder.Value} value
 * @param {(uuid: string, repo: string|null) => void} onOpen
 */
export function valueEl(value, onOpen) {
  /** @param {string} uuid @param {string|null} [repo] */
  const link = (uuid, repo = null) =>
    el(
      'a',
      {
        href: '#',
        class: 'ref-link',
        onclick: (/** @type {Event} */ event) => {
          event.preventDefault();
          onOpen(uuid, repo);
        },
      },
      uuid,
    );
  switch (value.type) {
    case 'ref':
    case 'refbase':
      return el('span', {}, link(value.value));
    case 'tree_ref':
      return el(
        'span',
        {},
        value.value.parent === null ? '(root)' : link(value.value.parent),
        ` / ${value.value.name}`,
      );
    case 'externalref':
      return el(
        'span',
        {},
        `${value.value.repo} :: `,
        link(value.value.metarecord, value.value.repo),
      );
    default:
      return el('span', {}, formatValue(value));
  }
}

// metafolder panel UI helpers — served at /__ui.js for panel types
// (spec-gui "The metafolder API"). Plain DOM building (no innerHTML:
// values come from user files, keep textContent semantics) and the
// canonical display form of the data model's Value variants.

/**
 * Builds a DOM element: el('td', { class: ['cell', active && 'cursor'],
 * onclick: handler }, children...).
 *
 * Props: `class` is a string or an array (falsy entries dropped); `on*`
 * keys attach event listeners; existing IDL properties (value, checked,
 * colSpan, hidden, ...) are assigned, anything else becomes an attribute.
 * Children: nested arrays are flattened; null/undefined/false are
 * skipped; strings become text nodes.
 */
export function el(tag, props = {}, ...children) {
  const element = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === 'class') {
      element.className = Array.isArray(value) ? value.filter(Boolean).join(' ') : value;
    } else if (key.startsWith('on')) {
      element.addEventListener(key.slice(2).toLowerCase(), value);
    } else if (key in element) {
      element[key] = value;
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
 * File extensions safe to display as an <img> thumbnail. Anything else must
 * NOT be handed to an <img>: pointing one at a video/pdf makes WebKit fetch
 * the whole file and try to decode it as an image, which balloons the web
 * process to gigabytes and crashes it.
 */
export const THUMBNAILABLE = new Set([
  'png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'avif', 'ico',
]);

/** Whether a path/filename has an image extension safe for an <img> thumbnail. */
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

/** Whether a path/filename is a video the server can make a poster for. */
export function isVideoThumbnailable(pathOrName) {
  return VIDEO_THUMBNAILABLE.has(extensionOf(pathOrName));
}

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
 */
export function fileTypeGlyph(pathOrName, fallback = '📄') {
  return FILE_TYPE_GLYPHS.get(extensionOf(pathOrName)) ?? fallback;
}

/**
 * Builds a thumbnail node for a filesystem entry, shared by every panel that
 * shows files (file, metarecord-list) so the "never put a non-image in an
 * <img>" rule lives in one place. Image files become a lazy <img> at /fsraw;
 * videos a lazy <img> at /thumbnail (a server-extracted poster frame — never
 * the raw file). Both are bare <img>s, so each panel's own `.thumb img` /
 * `.card img` CSS styles them. Directories, every other type, and a failed
 * image/poster load fall back to a type glyph <span> (see `fileTypeGlyph`).
 *
 * Options: `isDir`, `dirGlyph` (default 📁), `fileGlyph` (fallback for an
 * unknown type, default 📄), `glyphClass` (CSS class for the glyph span),
 * `token` (the session token appended as `?token=`, required for the protected
 * `/fsraw` and `/thumbnail` routes — spec-auth).
 */
export function thumbnail(guiServer, path, options = {}) {
  const { isDir = false, dirGlyph = '📁', fileGlyph = '📄', glyphClass = '', token = '' } = options;
  const glyph = (text) => el('span', glyphClass ? { class: glyphClass } : {}, text);
  if (isDir) return glyph(dirGlyph);
  if (!path) return glyph(fileGlyph);
  const image = isThumbnailable(path);
  if (image || isVideoThumbnailable(path)) {
    const endpoint = image ? 'fsraw' : 'thumbnail';
    const auth = token ? `&token=${encodeURIComponent(token)}` : '';
    return el('img', {
      loading: 'lazy',
      src: `${guiServer}/${endpoint}?path=${encodeURIComponent(path)}${auth}`,
      onerror: (event) => event.target.replaceWith(glyph(fileTypeGlyph(path, fileGlyph))),
    });
  }
  return glyph(fileTypeGlyph(path, fileGlyph));
}

/** Display form of a Value ({type, value} JSON, spec-data-model). */
export function formatValue({ type, value }) {
  switch (type) {
    case 'nothing':
      return '∅';
    case 'tree_ref':
      return `${value.parent ?? '(root)'} / ${value.name}`;
    case 'externalref':
      return `${value.repo} :: ${value.metarecord}`;
    default:
      return String(value);
  }
}

// Memoized `name -> field[]` index per metarecord object (built once, reused by
// every cell). The WeakMap is keyed by the metarecord, so entries vanish when a
// result set is dropped.
const byNameCache = new WeakMap();

/** A `name -> field[]` map of a metarecord's fields (a multi-map), built once. */
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

/** First field of an metarecord with the given name (fields are a multi-map). */
export function field(metarecord, name) {
  return byName(metarecord).get(name)?.[0];
}

/** Every field of an metarecord with the given name (multi-map rows, in order). */
export function fields(metarecord, name) {
  return byName(metarecord).get(name) ?? [];
}

/**
 * Like formatValue, but as a DOM node where metarecord references are
 * clickable: the uuid of ref/refbase, the parent of a tree_ref, the
 * metarecord of an externalref. Clicking calls onOpen(uuid, repo) — repo is
 * null except for externalref (the referenced metarecord's repository).
 */
export function valueEl(value, onOpen) {
  const link = (uuid, repo = null) =>
    el(
      'a',
      {
        href: '#',
        class: 'ref-link',
        onclick: (event) => {
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

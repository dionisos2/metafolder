// metafolder panel UI helpers — served at /__ui.js for panel types
// (doc "The metafolder API"). Plain DOM building (no innerHTML:
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
 * @typedef {Node|string|number|false|null|undefined|{text: string, bytes: string}|any[]} ElChild
 */

/**
 * Builds a DOM element: el('td', { class: ['cell', active && 'cursor'],
 * onclick: handler }, children...).
 *
 * Props: `class` is a string or an array (falsy entries dropped); `on*`
 * keys attach event listeners; existing IDL properties (value, checked,
 * colSpan, hidden, ...) are assigned, anything else becomes an attribute.
 * Children: nested arrays are flattened; null/undefined/false are
 * skipped; strings become text nodes, and so does a path that is not text
 * (`{text, bytes}`, doc "Tree names"), by its text. As a prop value, such a
 * path sets the attribute to its text and `<name>-bytes` to its bytes
 * (`datasetPath` reads the pair back).
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
    if (isBytePath(value)) {
      // A path that is not text (doc "Tree names"): the attribute shows its
      // text, and `<key>-bytes` keeps what it is (read back by `datasetPath`).
      element.setAttribute(key, value.text);
      element.setAttribute(`${key}-bytes`, value.bytes);
    } else if (key === 'class') {
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
    ...children
      .flat(Infinity)
      .filter((c) => c !== null && c !== undefined && c !== false)
      // A path that is not text shows as its text, never "[object Object]".
      .map((c) => (isBytePath(c) ? c.text : c)),
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
 * Extensions a tile takes for a video, and so points at `GET /thumbnail` for a
 * poster frame. A *hint* for which URL a tile tries, nothing more: the server
 * reads the file's content and answers 415 for what is not a video (the tile
 * then shows its glyph). A tile cannot ask first — a grid would pay one
 * round-trip per entry.
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

/**
 * Extensions a tile takes for a document, whose first-page poster the GUI
 * server renders (`GET /thumbnail`, via poppler) — a hint like the video one
 * above, the server deciding from the content. Like a video, a document must NEVER be handed to
 * `<img src=/fsraw>` — WebKit would try to decode the file as an image.
 */
export const DOCUMENT_THUMBNAILABLE = new Set(['pdf']);

/**
 * Whether a path/filename is a document the server can make a poster for.
 * @param {string} pathOrName
 */
export function isDocumentThumbnailable(pathOrName) {
  return DOCUMENT_THUMBNAILABLE.has(extensionOf(pathOrName));
}

/** @param {string} pathOrName */
function extensionOf(pathOrName) {
  return (pathOrName.split('.').pop() ?? '').toLowerCase();
}

/**
 * How many leading bytes `looksLikeText` inspects. git's own binary check uses
 * the first ~8000 bytes; a prefix this size decides every real file while
 * keeping the scan trivial even when the caller passes a larger buffer.
 */
export const TEXT_SNIFF_WINDOW = 8192;

/**
 * Content sniff: does this byte prefix look like a text file we can show in a
 * `<pre>`? Used for files whose extension gives no answer (`.tid`, `.cmake`, a
 * script with no extension…), so an unlisted-but-plain-text file is previewed
 * instead of falling through to "no preview available".
 *
 * The heuristic is git's: a NUL byte (`0x00`) in the first `TEXT_SNIFF_WINDOW`
 * bytes means binary; anything else is text. Real binaries (images, video,
 * executables, archives) carry a NUL almost immediately, while text — ASCII,
 * UTF-8, or single-byte encodings like latin-1 — never does, so this admits
 * unknown text encodings without decoding them. Note the deliberate blind
 * spot: UTF-16/UTF-32 text *does* hold NULs and is reported binary here; such
 * files are meant to be recognised upstream by their explicit extension, never
 * by this sniff. An empty prefix counts as text (an empty file previews as an
 * empty document, not "no preview").
 *
 * @param {Uint8Array} bytes a prefix of the file (already length-capped by the caller)
 * @returns {boolean}
 */
export function looksLikeText(bytes) {
  const end = Math.min(bytes.length, TEXT_SNIFF_WINDOW);
  for (let i = 0; i < end; i++) {
    if (bytes[i] === 0) return false;
  }
  return true;
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
 * videos, GIFs and documents a lazy <img> at /thumbnail (a server-rendered
 * poster — never the raw video or PDF, a *still* first frame for a GIF so a
 * grid of GIFs does not turn into a wall of animations, and the first page for
 * a document). Both are bare <img>s, so
 * each panel's own `.thumb img` / `.card img` CSS styles them. Directories,
 * every other type, and a failed image/poster load fall back to a type glyph
 * <span> (see `fileTypeGlyph`) — except a GIF poster failure (ffmpeg missing,
 * file outside any repo), which retries the animated original at /fsraw
 * before the glyph: an animated thumbnail still beats no thumbnail.
 *
 * Options: `isDir`, `dirGlyph` (default 📁), `fileGlyph` (fallback for an
 * unknown type, default 📄), `glyphClass` (CSS class for the glyph span),
 * `token` (the session token appended as `?token=`, required for the protected
 * `/fsraw` and `/thumbnail` routes — doc "Session tokens").
 *
 * @param {string} guiServer
 * @param {WirePath|null|undefined} handle the file, exact (doc "Tree names"):
 *   its text says the type, its handle names it in the URL
 * @param {{isDir?: boolean, dirGlyph?: string, fileGlyph?: string,
 *          glyphClass?: string, token?: string}} [options]
 * @returns {HTMLElement}
 */
export function thumbnail(guiServer, handle, options = {}) {
  const { isDir = false, dirGlyph = '📁', fileGlyph = '📄', glyphClass = '', token = '' } = options;
  /** @param {string} text */
  const glyph = (text) => el('span', glyphClass ? { class: glyphClass } : {}, text);
  if (isDir) return glyph(dirGlyph);
  if (!handle) return glyph(fileGlyph);
  const path = pathText(handle);
  if (!path) return glyph(fileGlyph);
  const gif = extensionOf(path) === 'gif';
  const image = isThumbnailable(path);
  if (image || isVideoThumbnailable(path) || isDocumentThumbnailable(path)) {
    const endpoint = image && !gif ? 'fsraw' : 'thumbnail';
    const auth = token ? `&token=${encodeURIComponent(token)}` : '';
    const img = el('img', {
      loading: 'lazy',
      src: `${guiServer}/${endpoint}?${pathQuery(handle)}${auth}`,
    });
    const glyphFallback = () => img.replaceWith(glyph(fileTypeGlyph(path, fileGlyph)));
    img.onerror = !gif
      ? glyphFallback
      : () => {
          img.onerror = glyphFallback;
          img.src = `${guiServer}/fsraw?${pathQuery(handle)}${auth}`;
        };
    return img;
  }
  return glyph(fileTypeGlyph(path, fileGlyph));
}

/**
 * Whether a value type places a metarecord in a tree of named nodes: a
 * `tree_ref` (one position) or a `hierarchy_ref` (one row per parent, doc
 * "Hierarchy references") — what paths, parents and subtrees are read from.
 *
 * @param {string|null|undefined} type
 */
export function isPositionType(type) {
  return type === 'tree_ref' || type === 'hierarchy_ref';
}

/**
 * The Query IR matching the direct children of a node in `field`'s tree. The
 * node is addressed by a `uuid_in` sub-query (Follows matches metarecords whose
 * direct parent is in the set), which avoids building path strings and so is
 * robust to names containing "/". The roots of a tree are not reachable this
 * way (their parent is the root sentinel, not a real metarecord) — they come
 * from `GET …/tree/roots` instead.
 *
 * @param {string} field @param {string} parentUuid
 */
export function childrenQuery(field, parentUuid) {
  return { type: 'follows', field, target: { type: 'uuid_in', uuids: [parentUuid] } };
}

/**
 * The single name a metarecord bears in `field`'s tree (the first position row
 * of that field — a node of a hierarchy bears one name on all its rows), or
 * null when it carries no such position.
 *
 * @param {Metafolder.Metarecord|null|undefined} metarecord
 * @param {string} field
 * @returns {string|null}
 */
export function treeNameOf(metarecord, field) {
  for (const f of metarecord?.fields ?? []) {
    if (f.name !== field) continue;
    if (f.value.type === 'tree_ref' || f.value.type === 'hierarchy_ref') {
      return pathText(f.value.value.name);
    }
  }
  return null;
}

/**
 * The tree whose children the viewer shows for a metarecord that has no file
 * (doc "viewer panel"): `preferred` — the field chosen in `path-ref` — when the
 * metarecord is placed in it, else the first field it is placed in. Never
 * `mfr_path`: a metarecord placed there is a file, and shows its file.
 *
 * @param {Metafolder.Metarecord|null|undefined} metarecord
 * @param {unknown} preferred
 * @returns {string|null}
 */
export function nodeFieldOf(metarecord, preferred) {
  /** @type {string[]} */
  const placed = [];
  for (const f of metarecord?.fields ?? []) {
    if (f.name !== 'mfr_path' && isPositionType(f.value.type) && !placed.includes(f.name)) {
      placed.push(f.name);
    }
  }
  if (typeof preferred === 'string' && placed.includes(preferred)) return preferred;
  return placed[0] ?? null;
}

/**
 * Display form of a Value ({type, value} JSON, doc "Value types"). `value` is
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
    case 'hierarchy_ref':
      return `${field.value.parent ?? '(root)'} / ${pathText(field.value.name)}`;
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
    case 'hierarchy_ref':
      return el(
        'span',
        {},
        value.value.parent === null ? '(root)' : link(value.value.parent),
        ` / ${pathText(value.value.name)}`,
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

/** The icon of each kind of opening (doc "Opening a row"): going into the row
 *  (a folder, a tree node, a label) is not showing it in another panel, and
 *  the viewer panel (the row's content) is not the detail panel (its fields). */
const OPEN_GLYPHS = { enter: '↘', show: '↗', detail: 'ⓘ' };

/** U+FE0E asks for each glyph's text presentation: where the panel's font has
 *  no arrow, the fallback could otherwise be the emoji font, which draws `↘` as
 *  a blue square that the stylesheet's colour cannot recolour — and `↘` would
 *  then look exactly like `↗`. */
const TEXT_PRESENTATION = '\uFE0E';

/**
 * A row's open button (doc "Opening a row"). Opening is one click on it; a click
 * on the row itself only selects, so the row's text can be selected and the row
 * clicked as often as wanted. The press keeps the focus where it was (the
 * panel's keys keep working) and starts no text selection; the click is not
 * also a click on the row, so `onOpen` does the selecting it needs.
 *
 * @param {'enter'|'show'|'detail'} kind what opening does: go into the row, show
 *   it in the viewer panel, or in the detail panel
 * @param {string} title what opening does, for hover and screen readers
 * @param {() => unknown} onOpen
 * @param {{ disabled?: string }} [options] `disabled`: why there is nothing to
 *   open — the button is drawn greyed, with that as its title, and does nothing
 */
export function openButton(kind, title, onOpen, { disabled } = {}) {
  const label = disabled ?? title;
  return el(
    'button',
    {
      type: 'button',
      class: ['mf-open', `mf-open-${kind}`, ...(disabled ? ['mf-open-disabled'] : [])],
      title: label,
      'aria-label': label,
      tabIndex: -1,
      disabled: Boolean(disabled),
      onmousedown: (/** @type {MouseEvent} */ event) => event.preventDefault(),
      onclick: (/** @type {MouseEvent} */ event) => {
        event.stopPropagation();
        if (!disabled) void onOpen();
      },
    },
    OPEN_GLYPHS[kind] + TEXT_PRESENTATION,
  );
}

/**
 * The two open buttons of a row showing a metarecord or a path (doc "Opening a
 * row"): its content in the viewer panel (`↗` — or `↘`, going into it, for a
 * folder the panel lists itself), then its fields in the detail panel (`ⓘ`).
 * Each is greyed when the row has nothing for that viewer: no file or folder,
 * or no metarecord (an untracked entry). `open` receives what was asked —
 * `'viewer'`, `'metarecord-detail'` or `'enter'` — and does the selecting first.
 *
 * @param {{ hasFile: boolean, hasMetarecord: boolean, enter?: boolean,
 *   open: (what: 'viewer'|'metarecord-detail'|'enter') => unknown }} args
 * @returns {HTMLButtonElement[]}
 */
export function viewerButtons({ hasFile, hasMetarecord, enter = false, open }) {
  return [
    enter
      ? openButton('enter', 'Open this folder', () => open('enter'))
      : openButton('show', 'Show in the viewer panel, in the other slot', () => open('viewer'), {
          disabled: hasFile ? undefined : 'No file or folder to show',
        }),
    openButton('detail', 'Show in the detail panel, in the other slot', () => open('metarecord-detail'), {
      disabled: hasMetarecord ? undefined : 'No metarecord: record it first',
    }),
  ];
}

/** The first argument of a command that opens a panel type (doc "Panel slots
 *  and layout"): where — `here`, `other` (this workspace in the other slot) or
 *  `new` (a copy of it). Never pre-filled: the default target is the
 *  keybindings', and a command called by hand asks with an empty line. The
 *  command reads what it needs where it is, then does the rest at the target
 *  with `metafolder.atTarget`. (The shipped `commands.js` has the same helper.) */
export function whereArg() {
  return {
    name: 'where',
    prompt: () => 'Open where? (here / other / new)',
    complete: () => ['here', 'other', 'new'],
  };
}

/** The last argument of the commands that show a query in the metarecord
 *  list: `replace` (the default) makes it the whole query, `and` AND-s it with
 *  the simplified query in place (doc "metarecord-list panel"). */
export function queryModeArg() {
  return {
    name: 'mode',
    optional: true,
    prompt: () => 'Replace the query, or AND with it? (replace / and)',
    complete: () => ['replace', 'and'],
  };
}

/**
 * The terms of a normal query's top-level AND, each as written. A query holding
 * a top-level OR is one term, parenthesized so that an AND can follow it. Reads
 * the text without the parser: a term is told by its spelling.
 * @param {string} query
 */
export function queryTerms(query) {
  const text = query.trim();
  if (text === '') return [];
  /** @type {string[]} */
  const terms = [];
  let start = 0;
  let depth = 0;
  let hasOr = false;
  for (let i = 0; i < text.length; i += 1) {
    const c = text[i];
    if (c === '"') {
      // To the closing quote; a backslash takes the next character with it.
      for (i += 1; i < text.length && text[i] !== '"'; i += 1) if (text[i] === '\\') i += 1;
    } else if (c === '(') {
      depth += 1;
    } else if (c === ')') {
      depth -= 1;
    } else if (/[\p{L}\p{N}_]/u.test(/** @type {string} */ (c))) {
      const word = /^[\p{L}\p{N}_]+/u.exec(text.slice(i))?.[0] ?? '';
      if (depth === 0 && word === 'OR') hasOr = true;
      if (depth === 0 && word === 'AND') {
        terms.push(text.slice(start, i).trim());
        start = i + word.length;
      }
      i += word.length - 1;
    }
  }
  if (hasOr) return [`(${text})`];
  terms.push(text.slice(start).trim());
  return terms;
}

/**
 * `query` AND `term`, a term the query already holds being left out.
 * @param {string} query
 * @param {string} term
 */
export function andQuery(query, term) {
  const terms = queryTerms(query);
  const added = queryTerms(term).filter((t) => !terms.includes(t));
  return [...terms, ...added].join(' AND ');
}

/** Refuses what is neither `replace` nor `and`. @param {string} mode */
export function requireQueryMode(mode) {
  if (mode !== 'replace' && mode !== 'and') {
    throw new Error(`unknown mode: "${mode}" (expected replace / and)`);
  }
}

/**
 * The metarecord list's query variables that show `query` in its normal DSL
 * zone: with `replace` it is the whole query, the simplified zone emptied; with
 * `and` it is added to `normal`, what the zone holds, the simplified zone kept.
 * @param {string} query
 * @param {string} [mode]
 * @param {string} [normal]
 * @returns {Record<string, string>}
 */
export function listQueryVariables(query, mode = 'replace', normal = '') {
  requireQueryMode(mode);
  return mode === 'and'
    ? { 'metarecord-list:normal-query': andQuery(normal, query) }
    : { 'metarecord-list:query': '', 'metarecord-list:normal-query': query };
}

/** An error's message, whatever was thrown.
 *  @param {unknown} error @returns {string} */
export function messageOf(error) {
  return error instanceof Error ? error.message : String(error);
}

/** A byte count as a short human size, base 1024 with one decimal above bytes
 *  — the spelling `mf duplicate` prints (`512B`, `2.0K`, `5.0M`).
 *  @param {number} bytes @returns {string} */
export function formatSize(bytes) {
  const units = ['B', 'K', 'M', 'G', 'T'];
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return i === 0 ? `${bytes}B` : `${v.toFixed(1)}${units[i]}`;
}

/** Coarse "how long ago" from a unix-ms timestamp (`5s ago` … `2d ago`); a
 *  future timestamp (clock skew) reads `0s ago`.
 *  @param {number} atMs @param {number} [now] @returns {string} */
export function formatAge(atMs, now = Date.now()) {
  const secs = Math.max(0, Math.floor((now - atMs) / 1000));
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
}

/** What a recently-viewed entry is worth now (doc "Frecent folders"): its rank —
 *  one per view, aged by the GUI — weighted by how long ago it was last viewed,
 *  with zoxide's weights: an hour ×4, a day ×2, a week ×½, older ×¼. Shared by
 *  the recent panel and the shipped `commands.js` picks, so they agree.
 *  @param {number} rank @param {string} viewedAt ISO-8601 @param {number} [now] unix ms
 *  @returns {number} */
export function frecency(rank, viewedAt, now = Date.now()) {
  const age = now - Date.parse(viewedAt);
  const hour = 3600 * 1000;
  if (age < hour) return rank * 4;
  if (age < 24 * hour) return rank * 2;
  if (age < 7 * 24 * hour) return rank / 2;
  return rank / 4;
}

/** A frecency score as shown: to one decimal (`4`, `3.3`).
 *  @param {number} score @returns {string} */
export function formatScore(score) {
  return String(Math.round(score * 10) / 10);
}

const MODIFIERS = ['ctrl', 'alt', 'shift', 'meta'];

/**
 * A chord as the engine spells it — lowercased, modifiers in the order
 * `ctrl+alt+shift+meta` (`keybindings.rs`, `normalize_chord`) — so that
 * `shift+ctrl+a` finds `ctrl+shift+a`. Lenient: a chord being typed
 * (`ctrl+`) keeps its pieces as they are.
 * @param {string} chord @returns {string}
 */
export function normalizeChord(chord) {
  const pieces = chord.toLowerCase().split('+');
  const modifiers = MODIFIERS.filter((m) => pieces.includes(m));
  const rest = pieces.filter((p) => !MODIFIERS.includes(p));
  return [...modifiers, ...rest].join('+');
}

/**
 * Whether `keys` start with the sequence typed: every chord typed but the
 * last is matched whole, the last one as a prefix — `g` finds `g g`, `ctrl`
 * every ctrl chord.
 * @param {string[]} keys @param {string[]} typed
 */
export function keysStartWith(keys, typed) {
  if (typed.length > keys.length) return false;
  return typed.every((chord, index) =>
    index < typed.length - 1 ? keys[index] === chord : keys[index].startsWith(chord),
  );
}

/**
 * The text a path or a name shows, as the daemon gives it (doc "Tree
 * names"): a plain string while it is text, `{text, bytes}` when it is not.
 * The GUI holds paths as the text it shows, so the object form is read as its
 * text — never dropped, never shown as an object. `undefined` for anything
 * else.
 * @param {unknown} value
 * @returns {string | undefined}
 */
export function wireText(value) {
  if (typeof value === 'string') return value;
  if (value && typeof value === 'object') {
    const text = /** @type {{ text?: unknown }} */ (value).text;
    if (typeof text === 'string') return text;
  }
  return undefined;
}

/**
 * The texts of a list of paths ({@link wireText}), skipping what is neither
 * form.
 * @param {unknown} values
 * @returns {string[]}
 */
export function wireTexts(values) {
  if (!Array.isArray(values)) return [];
  return values.map(wireText).filter((t) => t !== undefined);
}

// ── Exact paths (doc "Tree names") ──────────────────────────────────────────
//
// A path the GUI acts on is its exact bytes, in the daemon's wire form: a
// plain string while it is text — every path but a rare few, which the panels
// slice and join as strings —, `{text, bytes}` (bytes in lowercase hex) when it
// is not. The text is for showing; the handle goes back to the backend
// unchanged. These helpers do on bytes what a panel does on strings, and give
// back a string whenever the result is text, so a path that is text never
// becomes an object.

/** @typedef {string | {text: string, bytes: string}} WirePath */

const utf8 = new TextEncoder();
const strictUtf8 = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

/** Whether `value` is the object form of a path or a name.
 *  @param {unknown} value @returns {value is {text: string, bytes: string}} */
export function isBytePath(value) {
  return (
    value !== null &&
    typeof value === 'object' &&
    typeof (/** @type {{bytes?: unknown}} */ (value).bytes) === 'string' &&
    typeof (/** @type {{text?: unknown}} */ (value).text) === 'string'
  );
}

/** Lowercase hex of `bytes`. @param {Uint8Array} bytes */
function toHex(bytes) {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

/** The bytes a lowercase hex string spells. @param {string} hex */
function fromHex(hex) {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i += 1) out[i] = parseInt(hex.slice(2 * i, 2 * i + 2), 16);
  return out;
}

/** The exact bytes of a path: a string's UTF-8, an object's `bytes`.
 *  @param {WirePath} path @returns {Uint8Array} */
export function pathBytes(path) {
  return typeof path === 'string' ? utf8.encode(path) : fromHex(path.bytes);
}

/** The length of the valid UTF-8 sequence starting at `i`, 0 for none (the
 *  ranges Rust's `str::from_utf8` accepts: no overlong form, no surrogate,
 *  nothing past U+10FFFF). @param {Uint8Array} b @param {number} i */
function sequenceAt(b, i) {
  const lead = b[i];
  if (lead < 0x80) return 1;
  let length;
  let low = 0x80;
  let high = 0xbf;
  if (lead >= 0xc2 && lead <= 0xdf) length = 2;
  else if (lead >= 0xe0 && lead <= 0xef) {
    length = 3;
    if (lead === 0xe0) low = 0xa0;
    if (lead === 0xed) high = 0x9f;
  } else if (lead >= 0xf0 && lead <= 0xf4) {
    length = 4;
    if (lead === 0xf0) low = 0x90;
    if (lead === 0xf4) high = 0x8f;
  } else return 0;
  if (i + length > b.length || b[i + 1] < low || b[i + 1] > high) return 0;
  for (let k = 2; k < length; k += 1) if (b[i + k] < 0x80 || b[i + k] > 0xbf) return 0;
  return length;
}

/** The text of bytes as metafolder shows it: valid UTF-8 as is, each byte no
 *  text can represent as `%XX` — the daemon's own rendering.
 *  @param {Uint8Array} bytes */
export function escapeBytes(bytes) {
  let out = '';
  let run = 0;
  let i = 0;
  while (i < bytes.length) {
    const length = sequenceAt(bytes, i);
    if (length > 0) {
      i += length;
      continue;
    }
    out += strictUtf8.decode(bytes.subarray(run, i));
    out += `%${bytes[i].toString(16).toUpperCase().padStart(2, '0')}`;
    i += 1;
    run = i;
  }
  return out + strictUtf8.decode(bytes.subarray(run));
}

/** The wire form of these bytes: a string when they are text, else
 *  `{text, bytes}`. @param {Uint8Array} bytes @returns {WirePath} */
export function pathOfBytes(bytes) {
  try {
    return strictUtf8.decode(bytes);
  } catch {
    return { text: escapeBytes(bytes), bytes: toHex(bytes) };
  }
}

/** The text a path shows ({@link wireText} for a path known to be one).
 *  @param {WirePath} path @returns {string} */
export function pathText(path) {
  return typeof path === 'string' ? path : path.text;
}

/** A string that identifies a path exactly — a Map key, a Set member, an
 *  equality. Text is itself; bytes are prefixed with NUL, which no real path
 *  holds, so the two never meet. @param {WirePath} path @returns {string} */
export function pathKey(path) {
  return typeof path === 'string' ? path : `\u0000${path.bytes}`;
}

/** Whether two paths are the same bytes. @param {WirePath} a @param {WirePath} b */
export function samePath(a, b) {
  return pathKey(a) === pathKey(b);
}

/** The query parameter naming a path in a GUI server URL (`/fsraw`,
 *  `/thumbnail`, …): `path=` for text, `path_bytes=` (hex) otherwise.
 *  @param {WirePath} path */
export function pathQuery(path) {
  return typeof path === 'string'
    ? `path=${encodeURIComponent(path)}`
    : `path_bytes=${path.bytes}`;
}

/** The path a `data-*` attribute pair holds (`el` writes a byte path as the
 *  text plus `<name>-bytes`): `dataset.mfPath` / `dataset.mfPathBytes`.
 *  @param {DOMStringMap} dataset @param {string} key e.g. `mfPath`
 *  @returns {WirePath | undefined} */
export function datasetPath(dataset, key) {
  const text = dataset[key];
  if (typeof text !== 'string') return undefined;
  const bytes = dataset[`${key}Bytes`];
  return typeof bytes === 'string' && bytes !== '' ? { text, bytes } : text;
}

/**
 * A Map keyed by exact path ({@link pathKey}): two paths that show alike are
 * two keys, and iterating gives the paths back as they were set.
 * @template V
 */
export class PathMap {
  /** @param {Iterable<[WirePath, V]>} [entries] */
  constructor(entries = []) {
    /** @type {Map<string, [WirePath, V]>} */
    this.inner = new Map();
    for (const [path, value] of entries) this.set(path, value);
  }
  /** @param {WirePath} path */
  get(path) {
    return this.inner.get(pathKey(path))?.[1];
  }
  /** @param {WirePath} path */
  has(path) {
    return this.inner.has(pathKey(path));
  }
  /** @param {WirePath} path @param {V} value */
  set(path, value) {
    this.inner.set(pathKey(path), [path, value]);
    return this;
  }
  /** @param {WirePath} path */
  delete(path) {
    return this.inner.delete(pathKey(path));
  }
  get size() {
    return this.inner.size;
  }
  /** @returns {IterableIterator<[WirePath, V]>} */
  entries() {
    return this.inner.values();
  }
  /** @returns {IterableIterator<[WirePath, V]>} */
  [Symbol.iterator]() {
    return this.inner.values();
  }
  /** @returns {WirePath[]} */
  keys() {
    return [...this.inner.values()].map(([path]) => path);
  }
}

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

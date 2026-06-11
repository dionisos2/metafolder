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
      return `${value.repo} :: ${value.entry}`;
    default:
      return String(value);
  }
}

/** First field of an entry with the given name (fields are a multi-map). */
export function field(entry, name) {
  return (entry.fields ?? []).find((f) => f.name === name);
}

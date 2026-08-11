// Shared themed drop-down — served at /__select.js. Replaces the native
// <select>: WebKitGTK draws a native select's open options popup itself, so it
// ignores the panel theme and its scrollbar cannot be styled, and at the bottom
// of a panel the popup opens downward off-screen. This wires a <button> to the
// shared HTML menu (/__menu.js), which carries the theme (and our wider
// scrollbar), keyboard navigation and typeahead, and flips above the button
// when space runs out. The button styling (.mf-select) lives in the shipped
// style.css, so every panel gets a consistent themed control.

import { showMenu } from '/__menu.js';

/**
 * One drop-down option. `label` defaults to `value`; a `disabled` option is
 * shown greyed and cannot be chosen.
 * @typedef {{value: string, label?: string, disabled?: boolean}} SelectOption
 */

/**
 * Wires `button` as a drop-down over `options`, returning a small API to
 * read/set the selection and swap the option list. The button shows the
 * selected option's label (or `placeholder` when nothing matches) plus a
 * chevron; the open menu marks the current option with a trailing ✓ (kept
 * trailing so the menu's prefix typeahead still matches the plain label).
 *
 * @param {HTMLElement} button
 * @param {{options?: SelectOption[], value?: string|null, placeholder?: string,
 *          onChange?: (value: string) => void}} [config]
 */
export function createSelect(
  button,
  { options = [], value = null, placeholder = 'choose…', onChange = () => {} } = {},
) {
  /** @type {SelectOption[]} */
  let opts = options;
  /** @type {string|null} */
  let current = value;

  /** @param {string|null} v */
  const optionOf = (v) => opts.find((o) => o.value === v);
  /** @param {SelectOption} o */
  const labelOf = (o) => o.label ?? o.value;

  function render() {
    const selected = current === null ? undefined : optionOf(current);
    button.classList.add('mf-select');
    button.classList.toggle('mf-select-unset', selected === undefined);
    const label = document.createElement('span');
    label.className = 'mf-select-label';
    label.textContent = selected ? labelOf(selected) : placeholder;
    const chevron = document.createElement('span');
    chevron.className = 'mf-select-chevron';
    chevron.setAttribute('aria-hidden', 'true');
    chevron.textContent = '▾';
    button.replaceChildren(label, chevron);
  }
  render();

  button.addEventListener('click', () => {
    // Respect a disabled button (attribute or the HTMLButtonElement property).
    if (button.hasAttribute('disabled')) return;
    const rect = button.getBoundingClientRect();
    void showMenu(
      opts.map((o) => ({
        label: o.value === current ? `${labelOf(o)} ✓` : labelOf(o),
        disabled: o.disabled,
        action: () => set(o.value),
      })),
      { x: rect.left, y: rect.bottom + 2 },
    );
  });

  /** Sets the selection and fires onChange (unless unchanged). @param {string} v */
  function set(v) {
    if (v === current) return;
    current = v;
    render();
    onChange(v);
  }

  return {
    /** The selected value, or null when nothing is selected. */
    get: () => current,
    set,
    /** Sets the selection WITHOUT firing onChange (initialize / mirror external
     *  state — like assigning a native select's `.value`). @param {string|null} v */
    setValue: (v) => {
      current = v;
      render();
    },
    /** Replaces the option list, keeping the current selection when it still
     *  exists (else clearing to the placeholder). Does not fire onChange.
     *  @param {SelectOption[]} list @param {string|null} [keep] */
    setOptions: (list, keep = current) => {
      opts = list;
      current = keep !== null && list.some((o) => o.value === keep) ? keep : null;
      render();
    },
    element: button,
  };
}

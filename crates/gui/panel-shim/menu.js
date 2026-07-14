// metafolder HTML context menu — served at /__menu.js for panel types and
// imported by the shell and the shim (spec-gui "Context menus"). The
// native WebView menu (back/forward/...) is suppressed everywhere except
// editable text fields; panels build their own menus with showMenu (via
// metafolder.contextMenu).

// Default appearance, prepended to <head> so the user stylesheet
// (/__style.css, loaded after) wins on equal specificity.
const MENU_CSS = `
.mf-menu {
  position: fixed;
  z-index: 10000;
  min-width: 140px;
  padding: 4px 0;
  background: var(--mf-bg-raised, #26262e);
  color: var(--mf-fg, #d8d8e0);
  border: 1px solid var(--mf-border, #3a3a44);
  border-radius: 4px;
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.4);
  font-family: var(--mf-font, sans-serif);
  font-size: 13px;
  user-select: none;
}
.mf-menu-item {
  padding: 4px 14px;
  cursor: default;
  white-space: nowrap;
}
.mf-menu-item.active,
.mf-menu-item:not(.disabled):hover {
  background: var(--mf-accent, #3d5a80);
  color: var(--mf-fg-bright, #fff);
}
.mf-menu-item.disabled {
  color: var(--mf-fg-dim, #8a8a96);
}
.mf-menu-separator {
  margin: 4px 0;
  border-top: 1px solid var(--mf-border, #3a3a44);
}
`;

// At most one menu per document — but this module is evaluated twice in the
// same realm (bundled into the shell, and served as /__menu.js to panel
// code), so the open-menu handle must live on globalThis: the shell's
// hasOpenMenu() has to see a menu opened through the other instance, or its
// key matcher keeps firing bindings over an open panel menu.
/** @typedef {{close: (item: Metafolder.MenuEntry|null) => void}} MenuHandle */
const shared = /** @type {{__mfMenuState?: {active: MenuHandle|null}}} */ (
  /** @type {unknown} */ (globalThis)
).__mfMenuState ??= { active: null };

function installStyle() {
  if (document.getElementById('mf-menu-style')) return;
  const style = document.createElement('style');
  style.id = 'mf-menu-style';
  style.textContent = MENU_CSS;
  document.head.prepend(style);
}

/** Whether a menu is open: key handlers (shell and shim matchers) must
 *  stand down so the menu's own navigation receives the events. */
export function hasOpenMenu() {
  return shared.active !== null;
}

/**
 * Flips the menu to the other side of the anchor when it would overflow
 * the viewport; never returns a negative position.
 *
 * @param {number} x @param {number} y
 * @param {number} menuWidth @param {number} menuHeight
 * @param {number} viewportWidth @param {number} viewportHeight
 */
export function clampPosition(x, y, menuWidth, menuHeight, viewportWidth, viewportHeight) {
  let left = x;
  let top = y;
  if (left + menuWidth > viewportWidth) left = x - menuWidth;
  if (top + menuHeight > viewportHeight) top = y - menuHeight;
  return { x: Math.max(0, left), y: Math.max(0, top) };
}

/**
 * Shows an HTML context menu at {x, y} (viewport coordinates).
 *
 * `items` is an array of `{label, action?, disabled?}` objects and `'-'`
 * separators. Resolves with the chosen item (after calling its `action`)
 * or with null when dismissed (Escape, click outside, another menu).
 * Arrow keys navigate the enabled items (wrapping), Enter selects; typing
 * jumps to the first enabled item whose label starts with the typed prefix
 * (native-select typeahead: the buffer resets after a second's pause, and
 * repeating one letter cycles through its matches).
 *
 * @param {Metafolder.MenuItem[]} items
 * @param {{x: number, y: number}} position viewport coordinates
 * @returns {Promise<Metafolder.MenuEntry|null>}
 */
export function showMenu(items, { x, y }) {
  shared.active?.close(null);
  if (!items.some((item) => item !== '-')) return Promise.resolve(null);
  const enabled = /** @type {Metafolder.MenuEntry[]} */ (
    items.filter((item) => item !== '-' && !item.disabled)
  );
  installStyle();

  return new Promise((resolve) => {
    const menu = document.createElement('div');
    menu.className = 'mf-menu';
    menu.setAttribute('role', 'menu');

    let activeIndex = -1; // index into `enabled`
    /** @type {Map<Metafolder.MenuEntry, HTMLElement>} */
    const itemElements = new Map(); // enabled item -> element

    for (const item of items) {
      if (item === '-') {
        const separator = document.createElement('div');
        separator.className = 'mf-menu-separator';
        menu.append(separator);
        continue;
      }
      const element = document.createElement('div');
      element.className = item.disabled ? 'mf-menu-item disabled' : 'mf-menu-item';
      element.setAttribute('role', 'menuitem');
      element.textContent = item.label;
      if (!item.disabled) {
        itemElements.set(item, element);
        element.addEventListener('click', () => close(item));
      }
      menu.append(element);
    }

    /** @param {number} index index into `enabled` */
    function setActive(index) {
      const previous = itemElements.get(enabled[activeIndex]);
      previous?.classList.remove('active');
      activeIndex = index;
      itemElements.get(enabled[activeIndex])?.classList.add('active');
    }

    /** @param {Metafolder.MenuEntry|null} item the chosen item, null on dismissal */
    function close(item) {
      if (shared.active === handle) shared.active = null;
      menu.remove();
      window.removeEventListener('keydown', /** @type {EventListener} */ (onKeydown), {
        capture: true,
      });
      window.removeEventListener('mousedown', /** @type {EventListener} */ (onMousedown), {
        capture: true,
      });
      item?.action?.();
      resolve(item);
    }
    const handle = { close };

    // Typeahead buffer: printable keys accumulate for a second, then reset.
    let typed = '';
    let typedAt = 0;

    /** @param {string} char one printable character */
    function typeahead(char) {
      const now = Date.now();
      if (now - typedAt > 1000) typed = '';
      typedAt = now;
      typed += char.toLowerCase();
      // Repeating one letter cycles through its matches; otherwise the whole
      // buffer is a prefix and the current item keeps the highlight while it
      // still matches.
      const cycling = typed.length > 1 && [...typed].every((c) => c === typed[0]);
      const needle = cycling ? typed[0] : typed;
      const from = activeIndex < 0 ? 0 : cycling ? activeIndex + 1 : activeIndex;
      for (let step = 0; step < enabled.length; step++) {
        const index = (from + step) % enabled.length;
        if (String(enabled[index].label).toLowerCase().startsWith(needle)) {
          setActive(index);
          return;
        }
      }
    }

    /** @param {KeyboardEvent} event */
    function onKeydown(event) {
      switch (event.key) {
        case 'Escape':
          close(null);
          break;
        case 'ArrowDown':
          if (enabled.length > 0) setActive((activeIndex + 1) % enabled.length);
          break;
        case 'ArrowUp':
          if (enabled.length > 0) setActive((activeIndex - 1 + enabled.length) % enabled.length);
          break;
        case 'Enter':
          close(activeIndex >= 0 ? enabled[activeIndex] : null);
          break;
        default:
          // Printable keys feed the typeahead and are swallowed either way
          // (an open menu is modal); modified keys pass through untouched.
          if (event.key.length !== 1 || event.ctrlKey || event.altKey || event.metaKey) return;
          typeahead(event.key);
          break;
      }
      event.preventDefault();
      event.stopPropagation();
    }

    /** @param {MouseEvent} event */
    function onMousedown(event) {
      if (!menu.contains(/** @type {Node|null} */ (event.target))) close(null);
    }

    window.addEventListener('keydown', /** @type {EventListener} */ (onKeydown), { capture: true });
    window.addEventListener('mousedown', /** @type {EventListener} */ (onMousedown), {
      capture: true,
    });
    shared.active = handle;

    document.body.append(menu);
    const position = clampPosition(
      x,
      y,
      menu.offsetWidth,
      menu.offsetHeight,
      window.innerWidth,
      window.innerHeight,
    );
    menu.style.left = `${position.x}px`;
    menu.style.top = `${position.y}px`;
  });
}

/** Editable text fields keep the WebView's native menu: cut/copy/paste
 *  has no cheap HTML replacement.
 *  @param {EventTarget|null} element the event's target */
function keepsNativeMenu(element) {
  if (!(element instanceof HTMLElement)) return false;
  return (
    ((element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement) &&
      !element.disabled &&
      !element.readOnly) ||
    element.isContentEditable
  );
}

/**
 * Suppresses the WebView's native context menu (back/forward/...) on the
 * whole document, except over editable text fields, which keep the native
 * cut/copy/paste menu. Panel right-click handlers still fire: an HTML
 * menu shown by showMenu replaces the native one.
 */
export function installContextMenuSuppression(target = window) {
  target.addEventListener('contextmenu', (event) => {
    if (!keepsNativeMenu(event.target)) event.preventDefault();
  });
}

/** @param {string} text */
function copyText(text) {
  if (navigator.clipboard?.writeText) return navigator.clipboard.writeText(text);
  // Non-secure-context fallback: execCommand needs a live selection, and
  // opening the menu may have cleared the original one.
  const area = document.createElement('textarea');
  area.value = text;
  document.body.append(area);
  area.select();
  document.execCommand('copy');
  area.remove();
}

/**
 * Installs the default context menu (spec-gui "Context menus"): right-click
 * anywhere that is not an editable text field and where no more specific
 * menu opened shows Copy (the selection, captured at open time) and the
 * everyday layout commands, sent through `dispatch(invocation)`.
 *
 * Returns `{ addItems, uninstall }`; `addItems` registers a provider
 * (`event => items`) whose items appear above the defaults — panels extend
 * the menu through `metafolder.contextMenu.addDefaultItems`.
 *
 * @param {Window} target
 * @param {(invocation: string) => unknown} dispatch
 */
export function installDefaultContextMenu(target, dispatch) {
  /** @type {((event: MouseEvent) => Metafolder.MenuItem[])[]} */
  const providers = [];

  /** @param {MouseEvent} event */
  function onContextMenu(event) {
    if (keepsNativeMenu(event.target)) return;
    if (hasOpenMenu()) return; // a more specific handler already answered
    const selection = String(target.getSelection?.() ?? '');
    /** @type {Metafolder.MenuItem[]} */
    const items = [];
    for (const provider of providers) {
      const extra = provider(event) ?? [];
      if (extra.length > 0) items.push(...extra, '-');
    }
    items.push(
      { label: 'Copy', disabled: selection === '', action: () => void copyText(selection) },
      '-',
      { label: 'Split / unsplit', action: () => void dispatch('panel:split-toggle') },
      { label: 'Swap panel types', action: () => void dispatch('panel:swap') },
      '-',
      { label: 'Open web inspector', action: () => void dispatch('devtools:open') },
    );
    void showMenu(items, { x: event.clientX, y: event.clientY });
  }

  target.addEventListener('contextmenu', /** @type {EventListener} */ (onContextMenu));
  return {
    addItems: (/** @type {(event: MouseEvent) => Metafolder.MenuItem[]} */ provider) =>
      providers.push(provider),
    uninstall: () =>
      target.removeEventListener('contextmenu', /** @type {EventListener} */ (onContextMenu)),
  };
}

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

let activeMenu = null; // { close(item|null) } — at most one menu per document

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
  return activeMenu !== null;
}

/** Flips the menu to the other side of the anchor when it would overflow
 *  the viewport; never returns a negative position. */
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
 * Arrow keys navigate the enabled items (wrapping), Enter selects.
 */
export function showMenu(items, { x, y }) {
  activeMenu?.close(null);
  if (!items.some((item) => item !== '-')) return Promise.resolve(null);
  const enabled = items.filter((item) => item !== '-' && !item.disabled);
  installStyle();

  return new Promise((resolve) => {
    const menu = document.createElement('div');
    menu.className = 'mf-menu';
    menu.setAttribute('role', 'menu');

    let activeIndex = -1; // index into `enabled`
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

    function setActive(index) {
      const previous = itemElements.get(enabled[activeIndex]);
      previous?.classList.remove('active');
      activeIndex = index;
      itemElements.get(enabled[activeIndex])?.classList.add('active');
    }

    function close(item) {
      activeMenu = null;
      menu.remove();
      window.removeEventListener('keydown', onKeydown, { capture: true });
      window.removeEventListener('mousedown', onMousedown, { capture: true });
      item?.action?.();
      resolve(item);
    }

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
          return;
      }
      event.preventDefault();
      event.stopPropagation();
    }

    function onMousedown(event) {
      if (!menu.contains(event.target)) close(null);
    }

    window.addEventListener('keydown', onKeydown, { capture: true });
    window.addEventListener('mousedown', onMousedown, { capture: true });
    activeMenu = { close };

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

/**
 * Suppresses the WebView's native context menu (back/forward/...) on the
 * whole document, except over editable text fields, which keep the native
 * cut/copy/paste menu. Panel right-click handlers still fire: an HTML
 * menu shown by showMenu replaces the native one.
 */
export function installContextMenuSuppression(target = window) {
  target.addEventListener('contextmenu', (event) => {
    const element = event.target;
    const tag = element?.tagName;
    const editableText =
      ((tag === 'INPUT' || tag === 'TEXTAREA') && !element.disabled && !element.readOnly) ||
      element?.isContentEditable;
    if (!editableText) event.preventDefault();
  });
}

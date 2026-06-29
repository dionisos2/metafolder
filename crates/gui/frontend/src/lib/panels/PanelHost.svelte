<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke, listen } from '../ipc';
  import { dispatch, setPanelDispatch } from '../commands';
  import { addDefaultMenuItems } from '../keys';
  import { focusedWs, refreshCommands, slotPayload, store } from '../store.svelte';
  import { createPanelApi, type PanelApiInstance } from './api';
  import { helpCursorSheet } from '../cursor';
  import type { CommandDef, SlotId } from '../types';
  // @ts-expect-error plain-JS module shared with the (former) panel shim
  import { createVisibilityGate } from '../../../../panel-shim/visibility.js';

  let layer = $state<HTMLElement | null>(null);

  // One instance per (workspace, panel type), kept alive for the whole session
  // (state retention) and NEVER reparented. Each panel runs in the shell's JS
  // realm inside its own Shadow DOM root (CSS isolation); the metafolder API is
  // called directly (no postMessage). Hidden instances are display:none.
  interface PanelInstance {
    wsId: string;
    panelType: string;
    host: HTMLDivElement;
    shadow: ShadowRoot;
    apiInst: PanelApiInstance;
    cleanup: (() => void) | null;
    mounted: Promise<void>;
  }
  const instances = new Map<string, PanelInstance>();
  // Panel command handlers, keyed `${wsId}|${panelType}|${name}`.
  const panelHandlers = new Map<string, (...args: string[]) => unknown>();
  let visibleSlots = new Map<string, SlotId>(); // instance key -> slot

  const base = `http://127.0.0.1:${store.guiPort}`;
  // Cache-bust modules once per session so an edited panel reloads on GUI
  // restart (a given URL's module is evaluated once per page).
  const bust = `?v=${Date.now()}`;

  // The user stylesheet, adopted (live) by every panel shadow root, plus a
  // base sheet sizing the body stand-in.
  const userSheet = new CSSStyleSheet();
  const baseSheet = new CSSStyleSheet();
  baseSheet.replaceSync(':host{display:block;height:100%}.mf-panel-body{width:100%;height:100%;box-sizing:border-box}');

  const instanceKey = (wsId: string, panelType: string) => `${wsId}|${panelType}`;

  function ensureInstance(wsId: string, panelType: string): PanelInstance {
    const key = instanceKey(wsId, panelType);
    const existing = instances.get(key);
    if (existing || !layer) return existing!;

    const host = document.createElement('div');
    host.className = 'panel-host';
    host.style.display = 'none';
    layer.appendChild(host);
    const shadow = host.attachShadow({ mode: 'open' });
    // The panel's own sheet is adopted last (in mountPanel) so it overrides the
    // theme, matching the old linked-theme-then-panel cascade order.

    const visibilityGate = createVisibilityGate();
    const apiInst = createPanelApi(
      {
        invoke,
        dispatch,
        registerHandler: (name, handler) => panelHandlers.set(`${key}|${name}`, handler),
        onCommandsChanged: () => void refreshCommands(),
        addDefaultMenuItems,
      },
      {
        wsId,
        panelType,
        guiServer: base,
        sessionToken: store.sessionToken,
        pageSize: store.pageSizes[panelType],
        root: shadow,
        visibilityGate,
      },
    );

    // Clicking into a panel focuses its slot. focusin only fires when a
    // focusable element (button, input, [tabindex]) receives focus, so a click
    // on plain content (text, an image, a bare div) would never focus the slot;
    // pointerdown covers every click whatever it lands on. Hosts are overlaid
    // in a separate layer, not children of the <section class="slot">, so the
    // slot's own onclick never sees these clicks. A focusable target raises
    // BOTH events, so focus_slot must be idempotent (focusing a fixed slot) —
    // the toggling panel_focus_next would cancel itself out and leave focus on
    // the original panel (the "clicking a button doesn't focus the panel" bug).
    const focusSlot = () => {
      const slot = visibleSlots.get(key);
      if (slot && slot !== store.layout.focused) void invoke('focus_slot', { slot });
    };
    host.addEventListener('focusin', focusSlot);
    host.addEventListener('pointerdown', focusSlot);

    const instance: PanelInstance = { wsId, panelType, host, shadow, apiInst, cleanup: null, mounted: undefined! };
    instances.set(key, instance);
    instance.mounted = mountPanel(instance);
    return instance;
  }

  async function mountPanel(instance: PanelInstance) {
    const { panelType, shadow } = instance;
    try {
      const html = await fetch(`${base}/panel/${panelType}/index.html`).then((r) => r.text());
      const doc = new DOMParser().parseFromString(html, 'text/html');
      // The panel's CSS as a constructed sheet, adopted AFTER the theme so the
      // panel's rules win (e.g. button colour over the theme's native-button
      // black) — the same precedence the old iframe linked-theme-then-panel
      // order gave. Built-in panels use no @import / relative url().
      const panelSheet = new CSSStyleSheet();
      try {
        panelSheet.replaceSync([...doc.querySelectorAll('style')].map((s) => s.textContent).join('\n'));
      } catch {
        /* malformed panel CSS: skip it rather than break the mount */
      }
      // helpCursorSheet is last so its `!important` `?` cursor wins while the
      // help-cursor mode is armed (empty otherwise).
      shadow.adoptedStyleSheets = [baseSheet, userSheet, panelSheet, helpCursorSheet];
      const body = document.createElement('div');
      body.className = 'mf-panel-body';
      for (const child of [...doc.body.childNodes]) {
        if (child.nodeName === 'SCRIPT' || child.nodeName === 'STYLE') continue;
        body.append(child); // logic lives in main.js::mount; CSS in panelSheet
      }
      shadow.append(body);
      const mod = await import(/* @vite-ignore */ `${base}/panel/${panelType}/main.js${bust}`);
      const ret = await mod.mount(shadow, instance.apiInst.api);
      instance.cleanup = typeof ret === 'function' ? ret : null;
      void invoke('panel_ready', { wsId: instance.wsId, panelType });
    } catch (error) {
      // Error boundary: a panel that fails to load shows its error and does
      // not break the shell (no iframe/process isolation any more).
      const pre = document.createElement('pre');
      pre.textContent = `panel "${panelType}" failed to load:\n${String(error)}`;
      pre.style.cssText = 'color:#f88;padding:1em;white-space:pre-wrap;font:12px monospace';
      shadow.append(pre);
    }
  }

  function teardown(key: string, instance: PanelInstance) {
    try {
      instance.cleanup?.();
    } catch {
      /* a panel's cleanup must not block teardown */
    }
    for (const handlerKey of [...panelHandlers.keys()]) {
      if (handlerKey.startsWith(`${key}|`)) panelHandlers.delete(handlerKey);
    }
    instance.host.remove();
    instances.delete(key);
  }

  // Slot bodies shrink/grow when the command input or a second status bar
  // appear: follow their geometry, not just layout events.
  const resizeObserver =
    typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(() => sync());
  const observedBodies = new Set<Element>();

  /** Aligns the instance pool with the current layout and slot geometry. */
  function sync() {
    if (!layer) return;
    const wanted = new Map<string, SlotId>();
    for (const slot of ['left', 'right'] as SlotId[]) {
      const payload = slotPayload(slot);
      if (!payload.visible || !payload.workspace_id || !payload.panel_type) continue;
      const instance = ensureInstance(payload.workspace_id, payload.panel_type);
      const body = document.querySelector(`[data-slot-body="${slot}"]`);
      if (!instance || !body) continue;
      if (resizeObserver && !observedBodies.has(body)) {
        resizeObserver.observe(body);
        observedBodies.add(body);
      }
      const rect = body.getBoundingClientRect();
      Object.assign(instance.host.style, {
        display: 'block',
        left: `${rect.left}px`,
        top: `${rect.top}px`,
        width: `${rect.width}px`,
        height: `${rect.height}px`,
      });
      wanted.set(instanceKey(payload.workspace_id, payload.panel_type), slot);
    }

    const liveWorkspaces = new Set(store.workspaces.map((w) => w.id));
    for (const [key, instance] of instances) {
      if (!liveWorkspaces.has(instance.wsId)) {
        teardown(key, instance); // workspace closed
        continue;
      }
      if (!wanted.has(key)) instance.host.style.display = 'none';
    }

    // Visibility pushes on change.
    for (const [key, slot] of wanted) {
      if (visibleSlots.get(key) !== slot) instances.get(key)?.apiInst.pushVisibility(true, slot);
    }
    for (const [key] of visibleSlots) {
      if (!wanted.has(key)) instances.get(key)?.apiInst.pushVisibility(false, null);
    }
    visibleSlots = wanted;
  }

  $effect(() => {
    // Re-position whenever layout, geometry, the tab set or fullscreen change.
    void store.layout;
    void store.splitRatio;
    void store.workspaces;
    void store.ui.fullscreen;
    requestAnimationFrame(sync);
  });

  onMount(() => {
    window.addEventListener('resize', sync);

    // Initialize the shared user stylesheet, kept live on style changes.
    void fetch(`${base}/__style.css`)
      .then((r) => r.text())
      .then((css) => userSheet.replaceSync(css))
      .catch(() => {});

    // Pre-instantiate every panel type once (hidden) so all panel commands are
    // registered session-wide. Delayed so the startup layout settles first.
    const prewarmTimer = setTimeout(() => {
      const wsId = focusedWs() ?? store.workspaces[0]?.id;
      if (!wsId) return;
      for (const panelType of store.panelTypes) ensureInstance(wsId, panelType);
    }, 1000);

    const unlisteners: Promise<() => void>[] = [
      listen<{ css: string }>('style-changed', (event) => userSheet.replaceSync(event.payload.css)),
      listen<{ workspace_id: string; key: string; value: unknown }>(
        'workspace-var-changed',
        (event) => {
          for (const instance of instances.values()) {
            if (instance.wsId === event.payload.workspace_id) {
              instance.apiInst.pushVarChanged(event.payload.key, event.payload.value);
            }
          }
        },
      ),
      listen<{ workspace_id: string; entry: unknown }>('message-appended', (event) => {
        for (const instance of instances.values()) {
          if (instance.wsId === event.payload.workspace_id) {
            instance.apiInst.pushMessageAppended(event.payload.entry);
          }
        }
      }),
    ];

    // Commands owned by panel types (spec-gui: lazy hidden instantiation;
    // reveal switches a slot to the owning panel type).
    setPanelDispatch(async (command: CommandDef, args: string[]) => {
      const wsId = focusedWs();
      if (!wsId || !command.owner) throw 'no workspace in the focused slot';
      const key = instanceKey(wsId, command.owner);

      if (command.reveal && !visibleSlots.has(key)) {
        const other: SlotId = store.layout.focused === 'left' ? 'right' : 'left';
        const otherPayload = slotPayload(other);
        const target =
          otherPayload.visible && otherPayload.workspace_id === wsId ? other : store.layout.focused;
        await invoke('panel_set_type', { slot: target, panelType: command.owner });
      }

      const instance = ensureInstance(wsId, command.owner);
      await instance.mounted;
      const handler = panelHandlers.get(`${key}|${command.name}`);
      if (!handler) throw `panel command ${command.name} has no handler`;
      await handler(...args);
    });

    return () => {
      clearTimeout(prewarmTimer);
      window.removeEventListener('resize', sync);
      resizeObserver?.disconnect();
      for (const unlisten of unlisteners) void unlisten.then((fn) => fn());
      setPanelDispatch(null);
    };
  });
</script>

<div class="panel-layer" bind:this={layer}></div>

<style>
  .panel-layer {
    position: fixed;
    inset: 0;
    pointer-events: none;
    z-index: 10;
  }
  .panel-layer :global(.panel-host) {
    position: absolute;
    pointer-events: auto;
    background: transparent;
    overflow: hidden;
  }
</style>

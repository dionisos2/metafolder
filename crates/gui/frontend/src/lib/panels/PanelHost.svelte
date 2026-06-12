<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke, listen } from '../ipc';
  import { dispatch, setPanelDispatch } from '../commands';
  import { focusedWs, refreshCommands, slotPayload, store } from '../store.svelte';
  import { createBridgeCore } from './bridge';
  import type { CommandDef, SlotId } from '../types';

  let layer = $state<HTMLElement | null>(null);

  // One iframe per (workspace, panel type), kept alive for the whole
  // session (state retention) and NEVER reparented — moving an iframe in
  // the DOM reloads it. Hidden instances are display:none.
  //
  // Instances are identified by the string key "ws|type". Incoming
  // messages are matched against the CURRENT contentWindow of each
  // iframe: WebKitGTK swaps the WindowProxy on cross-origin navigation,
  // so a proxy captured at creation would never equal event.source.
  const iframes = new Map<string, HTMLIFrameElement>();
  const readiness = new Map<string, { promise: Promise<void>; resolve: () => void }>();
  let visibleSlots = new Map<string, SlotId>(); // instance key -> slot

  const bridge = createBridgeCore({
    invoke,
    dispatch,
    onCommandsChanged: () => void refreshCommands(),
    onPendingKeys: (pending) => {
      store.ui.pendingKeys = pending;
    },
  });

  const instanceKey = (wsId: string, panelType: string) => `${wsId}|${panelType}`;

  function keyForSource(source: MessageEventSource): string | null {
    for (const [key, iframe] of iframes) {
      if (iframe.contentWindow === source) return key;
    }
    return null;
  }

  function ensureIframe(wsId: string, panelType: string): HTMLIFrameElement {
    const key = instanceKey(wsId, panelType);
    let iframe = iframes.get(key);
    if (iframe || !layer) return iframe!;

    iframe = document.createElement('iframe');
    iframe.className = 'panel-frame';
    iframe.src = `http://127.0.0.1:${store.guiPort}/panel/${panelType}/index.html`;
    iframe.style.display = 'none';
    layer.appendChild(iframe);
    iframes.set(key, iframe);

    let resolveReady = () => {};
    const promise = new Promise<void>((resolve) => (resolveReady = resolve));
    readiness.set(key, { promise, resolve: resolveReady });

    bridge.register(key, { wsId, panelType }, (message) =>
      iframe!.contentWindow?.postMessage(message, '*'),
    );
    return iframe;
  }

  async function sendInit(key: string) {
    const meta = bridge.instanceMeta(key);
    const target = iframes.get(key)?.contentWindow;
    if (!meta || !target) return;
    let vars: [string, unknown][] = [];
    try {
      vars = (await invoke('ws_vars', { wsId: meta.wsId })) as [string, unknown][];
    } catch {
      /* workspace may be gone already */
    }
    target.postMessage(
      {
        mf: true,
        type: 'init',
        workspaceId: meta.wsId,
        panelType: meta.panelType,
        slot: visibleSlots.get(key) ?? null,
        vars: Object.fromEntries(vars),
        // $state proxies cannot be structured-cloned: snapshot first.
        keytable: $state.snapshot(store.keytable),
        guiServer: `http://127.0.0.1:${store.guiPort}`,
      },
      '*',
    );
    readiness.get(key)?.resolve();
    // Visible in GET /gui/panels/:slot/view as "ready".
    void invoke('panel_ready', { wsId: meta.wsId, panelType: meta.panelType });
  }

  // The slot bodies shrink/grow when the command input, suggestions or a
  // second status bar appear: follow their geometry, not just layout
  // events.
  const resizeObserver =
    typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(() => sync());
  const observedBodies = new Set<Element>();

  /** Aligns the iframe pool with the current layout and slot geometry. */
  function sync() {
    if (!layer) return;
    const wanted = new Map<string, SlotId>();
    for (const slot of ['left', 'right'] as SlotId[]) {
      const payload = slotPayload(slot);
      if (!payload.visible || !payload.workspace_id || !payload.panel_type) continue;
      const iframe = ensureIframe(payload.workspace_id, payload.panel_type);
      const body = document.querySelector(`[data-slot-body="${slot}"]`);
      if (!iframe || !body) continue;
      if (resizeObserver && !observedBodies.has(body)) {
        resizeObserver.observe(body);
        observedBodies.add(body);
      }
      const rect = body.getBoundingClientRect();
      Object.assign(iframe.style, {
        display: 'block',
        left: `${rect.left}px`,
        top: `${rect.top}px`,
        width: `${rect.width}px`,
        height: `${rect.height}px`,
      });
      wanted.set(instanceKey(payload.workspace_id, payload.panel_type), slot);
    }

    const liveWorkspaces = new Set(store.workspaces.map((w) => w.id));
    for (const [key, iframe] of iframes) {
      const wsId = key.split('|')[0];
      if (!liveWorkspaces.has(wsId)) {
        // Workspace closed: drop the instance entirely.
        bridge.unregister(key);
        iframe.remove();
        iframes.delete(key);
        readiness.delete(key);
        continue;
      }
      if (!wanted.has(key)) iframe.style.display = 'none';
    }

    // Visibility pushes on change.
    for (const [key, slot] of wanted) {
      if (visibleSlots.get(key) !== slot) bridge.pushVisibility(key, true, slot);
    }
    for (const [key] of visibleSlots) {
      if (!wanted.has(key)) bridge.pushVisibility(key, false, null);
    }
    visibleSlots = wanted;
  }

  $effect(() => {
    // Re-position whenever layout, geometry or the tab set change.
    void store.layout;
    void store.splitRatio;
    void store.workspaces;
    requestAnimationFrame(sync);
  });

  onMount(() => {
    const onMessage = (event: MessageEvent) => {
      const data = event.data as { mf?: boolean; type?: string } | null;
      if (!data?.mf || !event.source) return;
      const key = keyForSource(event.source);
      if (!key) return;
      if (data.type === 'ready') {
        void sendInit(key);
        return;
      }
      if (data.type === 'focused') {
        const slot = visibleSlots.get(key);
        if (slot && slot !== store.layout.focused) void invoke('panel_focus_next');
        return;
      }
      void bridge.onMessage(key, data);
    };
    window.addEventListener('message', onMessage);
    window.addEventListener('resize', sync);

    // Pre-instantiate every panel type once (hidden, for the startup
    // workspace) so all panel commands are registered from the start:
    // dispatch by name then works session-wide (the registry survives
    // iframe removal). The shim's visibility gate keeps these instances
    // free of daemon traffic until actually displayed. Delayed so the
    // startup layout settles first.
    const prewarmTimer = setTimeout(() => {
      const wsId = focusedWs() ?? store.workspaces[0]?.id;
      if (!wsId) return;
      for (const panelType of store.panelTypes) ensureIframe(wsId, panelType);
    }, 1000);

    const unlisteners: Promise<() => void>[] = [
      listen<{ workspace_id: string; key: string; value: unknown }>(
        'workspace-var-changed',
        (event) =>
          bridge.forwardVarChange(
            event.payload.workspace_id,
            event.payload.key,
            event.payload.value,
          ),
      ),
      listen<{ workspace_id: string; entry: unknown }>('message-appended', (event) =>
        bridge.forwardMessageAppended(event.payload.workspace_id, event.payload.entry),
      ),
      listen<{ bindings: unknown }>('keybindings-changed', (event) =>
        bridge.pushKeytable(event.payload.bindings),
      ),
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
          otherPayload.visible && otherPayload.workspace_id === wsId
            ? other
            : store.layout.focused;
        await invoke('panel_set_type', { slot: target, panelType: command.owner });
      }

      ensureIframe(wsId, command.owner);
      await readiness.get(key)?.promise;
      await bridge.dispatchCommand(key, command.name, args);
    });

    return () => {
      clearTimeout(prewarmTimer);
      window.removeEventListener('message', onMessage);
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
  .panel-layer :global(.panel-frame) {
    position: absolute;
    border: none;
    pointer-events: auto;
    background: transparent;
  }
</style>

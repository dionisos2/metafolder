<script lang="ts">
  import { onMount } from 'svelte';
  import { initStore, store } from './lib/store.svelte';
  import { installKeys } from './lib/keys';
  import { installHelpCursorSheet } from './lib/cursor';
  import TabBar from './components/TabBar.svelte';
  import Slot from './components/Slot.svelte';
  import CommandInput from './components/CommandInput.svelte';
  import StatusBar from './components/StatusBar.svelte';
  import TaskBar from './components/TaskBar.svelte';
  import ConfigOverlay from './components/ConfigOverlay.svelte';
  import PanelHost from './lib/panels/PanelHost.svelte';

  let failure = $state<string | null>(null);
  let slotsElement = $state<HTMLElement | null>(null);
  let dragging = $state(false);

  onMount(async () => {
    try {
      await initStore();
      installHelpCursorSheet();
      installKeys();
    } catch (error) {
      failure = String(error);
    }
  });

  function onDividerDown(event: PointerEvent) {
    dragging = true;
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
  }

  function onDividerMove(event: PointerEvent) {
    if (!dragging || !slotsElement) return;
    const rect = slotsElement.getBoundingClientRect();
    const ratio = (event.clientX - rect.left) / rect.width;
    store.splitRatio = Math.min(0.85, Math.max(0.15, ratio));
  }

  const columns = $derived(
    store.layout.right.visible && store.layout.left.visible
      ? `${(store.splitRatio * 100).toFixed(2)}fr 5px ${((1 - store.splitRatio) * 100).toFixed(2)}fr`
      : '1fr',
  );
</script>

{#if failure}
  <div class="failure">Failed to initialize the GUI: {failure}</div>
{:else if store.ready}
  <div class="app">
    {#if store.ui.fullscreen}
      <!-- Immersive mode: only the focused panel, no chrome (escape exits). -->
      <div class="slots" style:grid-template-columns="1fr">
        <Slot id={store.layout.focused} chrome={false} />
      </div>
    {:else}
      {#if !store.daemonConnected}
        <div class="daemon-banner" data-help-topic="connection">
          Daemon unreachable at {store.daemonUrl} — daemon-dependent commands are disabled.
        </div>
      {/if}
      <TabBar />
      <div class="slots" bind:this={slotsElement} style:grid-template-columns={columns}>
        {#if store.layout.left.visible}
          <Slot id="left" />
        {/if}
        {#if store.layout.left.visible && store.layout.right.visible}
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <div
            class="divider"
            data-help-topic="layout"
            onpointerdown={onDividerDown}
            onpointermove={onDividerMove}
            onpointerup={() => (dragging = false)}
          ></div>
        {/if}
        {#if store.layout.right.visible}
          <Slot id="right" />
        {/if}
      </div>
      <CommandInput />
      <TaskBar />
      <StatusBar />
    {/if}
  </div>
  <PanelHost />
  {#if store.ui.configOpen}
    <ConfigOverlay />
  {/if}
{:else}
  <div class="loading">Loading…</div>
{/if}

<style>
  :global(html, body, #app) {
    height: 100%;
    margin: 0;
  }
  .app {
    display: flex;
    flex-direction: column;
    height: 100%;
    background: var(--mf-bg, #1e1e24);
    color: var(--mf-fg, #d8d8e0);
    font-family: var(--mf-font, sans-serif);
  }
  .slots {
    flex: 1;
    display: grid;
    min-height: 0;
  }
  .divider {
    cursor: col-resize;
    background: var(--mf-bg-raised, #26262e);
  }
  .daemon-banner {
    flex: none;
    background: var(--mf-error, #c44c56);
    color: #fff;
    padding: 4px 10px;
    text-align: center;
  }
  .loading,
  .failure {
    display: flex;
    height: 100vh;
    align-items: center;
    justify-content: center;
    color: var(--mf-fg-dim, #8a8a96);
  }
  .failure {
    color: var(--mf-error, #c44c56);
  }
</style>

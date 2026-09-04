<script lang="ts">
  // Dedicated bar for in-flight daemon tasks (spec-tasks "GUI"). It polls
  // GET /tasks (all loaded repos) and shows running/pending tasks with a
  // determinate progress bar when counts are known, a spinner otherwise.
  // Kept separate from the status bar so progress never saturates it. These
  // tasks are blocking, so surfacing them repo-wide (not only GUI-launched
  // ones) is the point.
  import { onMount } from 'svelte';
  import { invoke } from '../lib/ipc';
  import { ownedByVisible, store, visibleWorkspaces } from '../lib/store.svelte';

  interface Task {
    id: string;
    kind: string;
    status: string;
    phase: string;
    done: number | null;
    total: number | null;
    repo_uuid: string;
  }

  let tasks = $state<Task[]>([]);
  const POLL_MS = 300;

  async function poll() {
    try {
      const res = (await invoke('daemon_request', {
        method: 'GET',
        path: '/tasks',
        body: null,
      })) as { status: number; body: unknown };
      tasks =
        res.status === 200 && Array.isArray(res.body)
          ? (res.body as Task[]).filter((t) => t.status === 'running' || t.status === 'pending')
          : [];
    } catch {
      tasks = [];
    }
  }

  onMount(() => {
    void poll();
    const timer = setInterval(poll, POLL_MS);
    return () => clearInterval(timer);
  });

  function label(t: Task): string {
    return t.phase ? `${t.kind} · ${t.phase}` : t.kind;
  }

  // Unlike daemon tasks, a script's entry belongs to the workspaces that script
  // owns — it is part of what the script displays, so it goes away with its tab
  // (spec-gui "Script session").
  const scripts = $derived(
    store.ui.scriptTasks.filter((s) => ownedByVisible(s.workspaces, visibleWorkspaces())),
  );
</script>

{#if tasks.length > 0 || scripts.length > 0}
  <div class="task-bar" data-help-topic="task-bar">
    {#each scripts as s (s.task)}
      <div class="task" class:waiting={s.waiting}>
        <span class="label">{s.label}{s.phase ? ` · ${s.phase}` : ''}</span>
        <!-- Blocked on an answer: the script is NOT working, and the difference
             is the whole point — the spinner is how one tells "the queries are
             still running" from "it is your turn". -->
        {#if s.waiting}
          <span class="awaiting">⏎ your answer</span>
          {#if s.done != null && s.total != null}
            <span class="counts">{s.done}/{s.total}</span>
          {/if}
        {:else if s.done != null && s.total != null}
          <progress class="bar" value={s.done} max={s.total}></progress>
          <span class="counts">{s.done}/{s.total}</span>
        {:else}
          <span class="spinner"></span>
        {/if}
      </div>
    {/each}
    {#each tasks as t (t.id)}
      <div class="task">
        <span class="label">{label(t)}</span>
        {#if t.done != null && t.total != null}
          <progress class="bar" value={t.done} max={t.total}></progress>
          <span class="counts">{t.done}/{t.total}</span>
        {:else}
          <span class="spinner"></span>
        {/if}
      </div>
    {/each}
  </div>
{/if}

<style>
  .task-bar {
    display: flex;
    flex: none;
    flex-wrap: wrap;
    gap: 6px 18px;
    align-items: center;
    padding: 3px 10px;
    background: var(--mf-bg-raised, #26262e);
    border-top: 1px solid var(--mf-accent, #4c56c4);
    font-size: 0.85em;
  }
  .task {
    display: inline-flex;
    align-items: center;
    gap: 8px;
  }
  /* A waiting script is idle, not slow: no motion, and the label brightens so
     the eye lands on the entry whose question is on screen. */
  .task.waiting .label {
    color: var(--mf-fg, #d8d8e0);
  }
  .awaiting {
    color: var(--mf-accent, #4c56c4);
    font-weight: 600;
  }
  .label {
    color: var(--mf-fg-dim, #8a8a96);
    font-family: var(--mf-font-mono, monospace);
  }
  .counts {
    color: var(--mf-fg-dim, #8a8a96);
    font-variant-numeric: tabular-nums;
  }
  .bar {
    width: 8em;
    height: 0.7em;
    accent-color: var(--mf-accent, #4c56c4);
  }
  .bar::-webkit-progress-bar {
    background: var(--mf-bg, #1e1e24);
    border-radius: 3px;
  }
  .bar::-webkit-progress-value {
    background: var(--mf-accent, #4c56c4);
    border-radius: 3px;
  }
  .spinner {
    width: 0.8em;
    height: 0.8em;
    border: 2px solid var(--mf-fg-dim, #8a8a96);
    border-top-color: transparent;
    border-radius: 50%;
    animation: task-spin 0.8s linear infinite;
  }
  @keyframes task-spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>

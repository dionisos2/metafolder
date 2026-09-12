<script lang="ts">
  // Dedicated bar for in-flight daemon tasks (spec-tasks "GUI"). It polls
  // GET /tasks (all loaded repos) and shows running/pending tasks with a
  // determinate progress bar when counts are known, a spinner otherwise.
  // Kept separate from the status bar so progress never saturates it. These
  // tasks are blocking, so surfacing them repo-wide (not only GUI-launched
  // ones) is the point.
  import { onMount } from 'svelte';
  import { invoke } from '../lib/ipc';
  import { ownedByVisible, scriptIndicator, store, visibleWorkspaces } from '../lib/store.svelte';
  import { daemonWork, settledTasks, type WorkingState } from '../lib/working';

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

  // When each in-flight task was first seen, so a task too short to be worth
  // rearranging the screen for never reaches the bar (see lib/working). The
  // ordinary watcher flush is exactly that: milliseconds long, and fired by
  // anything at all touching a watched file. Bookkeeping for the poll, never
  // rendered — `tasks` is the reactive state — so a plain Map is right here.
  // eslint-disable-next-line svelte/prefer-svelte-reactivity
  const firstSeen = new Map<string, number>();

  async function poll() {
    try {
      const res = (await invoke('daemon_request', {
        method: 'GET',
        path: '/tasks',
        body: null,
      })) as { status: number; body: unknown };
      const active =
        res.status === 200 && Array.isArray(res.body)
          ? (res.body as Task[]).filter((t) => t.status === 'running' || t.status === 'pending')
          : [];
      tasks = settledTasks(firstSeen, active, Date.now());
    } catch {
      firstSeen.clear();
      tasks = [];
    }
  }

  // A daemon call the user is waiting on that is not an observable task — a
  // write, a query. Nothing shows for the ordinary fast ones; see lib/working.
  let working = $state<WorkingState | null>(null);

  onMount(() => {
    void poll();
    const timer = setInterval(poll, POLL_MS);
    const stopWatching = daemonWork.subscribe((state) => (working = state));
    return () => {
      clearInterval(timer);
      stopWatching();
    };
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

{#if tasks.length > 0 || scripts.length > 0 || working !== null}
  <div class="task-bar" data-help-topic="task-bar">
    {#if working !== null}
      <div class="task">
        <span class="label">{working.label}</span>
        <span class="spinner"></span>
        {#if working.count > 1}
          <span class="counts">+{working.count - 1}</span>
        {/if}
      </div>
    {/if}
    {#each scripts as s (s.task)}
      {@const ind = scriptIndicator(s)}
      <div class="task" class:waiting={s.waiting}>
        <span class="label">{s.label}{s.phase ? ` · ${s.phase}` : ''}</span>
        <!-- Blocked on an answer: the script is NOT working, and the difference
             is the whole point — the spinner is how one tells "the queries are
             still running" from "it is your turn". It therefore keeps spinning
             next to the determinate bar, which only moves between questions and
             would otherwise leave a working script looking idle. -->
        {#if ind.awaiting}
          <span class="awaiting">⏎ your answer</span>
        {:else}
          <span class="spinner"></span>
        {/if}
        {#if ind.bar}
          <progress class="bar" value={s.done} max={s.total}></progress>
        {/if}
        {#if ind.counts}
          <span class="counts">{s.done}/{s.total}</span>
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

<script lang="ts">
  // Dedicated bar for in-flight daemon tasks (doc "Following a task"). It polls
  // GET /tasks (all loaded repos) and shows running/pending tasks with a
  // determinate progress bar when counts are known, a spinner otherwise.
  // Kept separate from the status bar so progress never saturates it. These
  // tasks are blocking, so surfacing them whoever launched them (not only
  // GUI-launched ones) is the point — but only for the repositories of the
  // workspaces on screen, the ones whose actions queue behind them.
  import { onMount } from 'svelte';
  import { invoke } from '../lib/ipc';
  import {
    ownedByVisible,
    scriptIndicator,
    store,
    visibleWorkspaces,
    workspaceById,
  } from '../lib/store.svelte';
  import { jobs, shownJobs, type Job } from '../lib/jobs';
  import { bars } from '../lib/bars';
  import { dispatch } from '../lib/commands';
  import { daemonWork, settledTasks, tasksOfRepos, type WorkingState } from '../lib/working';

  interface Task {
    id: string;
    kind: string;
    status: string;
    phase: string;
    done: number | null;
    total: number | null;
    repo_uuid: string;
  }

  // Every settled task of every loaded repository, as polled; what is shown is
  // narrowed to the visible workspaces' repositories below, so switching a slot
  // shows a task at once rather than at the next poll's grace.
  let allTasks = $state<Task[]>([]);
  const tasks = $derived(
    tasksOfRepos(
      allTasks,
      visibleWorkspaces().map((ws) => workspaceById(ws)?.active_repo ?? null),
    ),
  );
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
      allTasks = settledTasks(firstSeen, active, Date.now());
    } catch {
      firstSeen.clear();
      allTasks = [];
    }
  }

  // A daemon call the user is waiting on that is not an observable task — a
  // write, a query. Nothing shows for the ordinary fast ones; see lib/working.
  let working = $state<WorkingState | null>(null);

  // The shell's own long commands (lib/jobs), shown once past their grace:
  // re-read on every change and on every poll, which is when the grace of a
  // job that has not changed runs out.
  let allJobs = $state<Job[]>([]);
  let now = $state(Date.now());
  const shown = $derived(shownJobs(allJobs, visibleWorkspaces(), now));

  onMount(() => {
    void poll();
    const timer = setInterval(() => {
      now = Date.now();
      void poll();
    }, POLL_MS);
    const stopWatching = daemonWork.subscribe((state) => (working = state));
    allJobs = jobs.list();
    const stopJobs = jobs.subscribe((list) => {
      allJobs = list;
      now = Date.now();
    });
    return () => {
      clearInterval(timer);
      stopWatching();
      stopJobs();
    };
  });

  function label(t: Task): string {
    return t.phase ? `${t.kind} · ${t.phase}` : t.kind;
  }

  // Unlike daemon tasks, a script's entry belongs to the workspaces that script
  // owns — it is part of what the script displays, so it goes away with its tab
  // (doc "Script sessions").
  const scripts = $derived(
    store.ui.scriptTasks.filter((s) => ownedByVisible(s.workspaces, visibleWorkspaces())),
  );

  // The bar takes its own place above the command input: shown from its first
  // entry on, kept once empty, hidden by the user, shown again by a new entry
  // (lib/bars, doc "Status bar and task bar").
  let shownBar = $state(bars.shown('tasks'));
  onMount(() => bars.subscribe((state) => (shownBar = state.tasks)));
  $effect(() => {
    bars.note('tasks', [
      ...(working !== null ? ['call'] : []),
      ...shown.map((j) => `job:${j.id}`),
      ...scripts.map((s) => `script:${s.task}`),
      ...tasks.map((t) => `task:${t.id}`),
    ]);
  });
  const empty = $derived(
    tasks.length === 0 && scripts.length === 0 && shown.length === 0 && working === null,
  );
</script>

{#if shownBar}
  <div class="task-bar" data-help-topic="task-bar">
    {#if empty}
      <span class="label">no task running</span>
    {/if}
    {#if working !== null}
      <div class="task">
        <span class="label">{working.label}</span>
        <span class="spinner"></span>
        {#if working.count > 1}
          <span class="counts">+{working.count - 1}</span>
        {/if}
      </div>
    {/if}
    {#each shown as j (j.id)}
      <div class="task">
        <span class="label">{j.label}{j.phase ? ` · ${j.phase}` : ''}</span>
        <span class="spinner"></span>
        {#if j.done != null && j.total != null}
          <progress class="bar" value={j.done} max={j.total}></progress>
          <span class="counts">{j.done}/{j.total}</span>
        {/if}
      </div>
    {/each}
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
    <button
      class="hide"
      title="Hide the task bar (bar:toggle tasks)"
      onclick={(event) => {
        // Hand the focus straight back, or the button would keep the keys.
        (event.currentTarget as HTMLElement).blur();
        void dispatch('bar:toggle tasks');
      }}>×</button
    >
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
  .hide {
    margin-left: auto;
    padding: 0 4px;
    border: none;
    background: none;
    color: var(--mf-fg-dim, #8a8a96);
    font: inherit;
    cursor: pointer;
  }
  .hide:hover {
    color: var(--mf-fg, #d8d8e0);
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

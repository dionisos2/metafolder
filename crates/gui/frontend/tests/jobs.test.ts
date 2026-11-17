// The shell's own long commands in the task bar (doc "Status bar and task bar"):
// a command that loops over many quick calls — none of them slow enough to be
// announced on its own — shows one entry for the whole run, with its counts.

import { describe, expect, test } from 'vitest';
import { createJobs, shownJobs } from '../src/lib/jobs';
import { WORKING_GRACE_MS } from '../src/lib/working';

describe('shell jobs', () => {
  test('a job is listed from its start to its end', () => {
    const jobs = createJobs();
    const job = jobs.start('ws-1', 'mf:duplicate keep');
    expect(jobs.list()).toEqual([
      {
        id: job.id,
        workspace: 'ws-1',
        label: 'mf:duplicate keep',
        phase: null,
        done: null,
        total: null,
        started: expect.any(Number),
      },
    ]);
    job.end();
    expect(jobs.list()).toEqual([]);
  });

  test('progress updates the entry, keeping what it does not name', () => {
    const jobs = createJobs();
    const job = jobs.start('ws-1', 'x');
    job.update({ phase: 'trashing', total: 4 });
    job.update({ done: 1 });
    expect(jobs.list()[0]).toMatchObject({ phase: 'trashing', done: 1, total: 4 });
  });

  test('listeners hear every change; ending twice is harmless', () => {
    const jobs = createJobs();
    const seen: number[] = [];
    jobs.subscribe((list) => seen.push(list.length));
    const job = jobs.start('ws-1', 'x');
    job.update({ done: 1 });
    job.end();
    job.end();
    job.update({ done: 2 });
    expect(seen).toEqual([1, 1, 0]);
    expect(jobs.list()).toEqual([]);
  });

  test('shownJobs keeps those that have run for the grace, on screen', () => {
    const job = (id: number, workspace: string, started: number) => ({
      id, workspace, label: 'x', phase: null, done: null, total: null, started,
    });
    const list = [job(0, 'ws-1', 1000), job(1, 'ws-1', 1200), job(2, 'ws-2', 1000)];
    expect(shownJobs(list, ['ws-1'], 1000 + WORKING_GRACE_MS).map((j) => j.id)).toEqual([0]);
    expect(shownJobs(list, ['ws-1', 'ws-2'], 1500).map((j) => j.id)).toEqual([0, 1, 2]);
    expect(shownJobs(list, ['ws-1'], 1000 + WORKING_GRACE_MS - 1)).toEqual([]);
  });

  test('run ends the job whether the work succeeds or throws', async () => {
    const jobs = createJobs();
    await jobs.run('ws-1', 'ok', async () => {
      expect(jobs.list()).toHaveLength(1);
    });
    await expect(
      jobs.run('ws-1', 'fails', async () => {
        throw new Error('boom');
      }),
    ).rejects.toThrow('boom');
    expect(jobs.list()).toEqual([]);
  });
});

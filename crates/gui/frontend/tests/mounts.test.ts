// Unmounted volumes (panel-shim/mounts.js, spec-gui "Unmounted volumes"): a
// metarecord whose file sits on a volume that is not plugged in is
// *unavailable*, not orphaned — the panels must tell the two apart.

import { describe, expect, test, vi } from 'vitest';
import {
  fetchMounts,
  offlineMountFor,
  relativeTo,
  unavailableLabel,
} from '../../panel-shim/mounts.js';

type Mount = Parameters<typeof unavailableLabel>[0];

const offline: Mount = {
  uuid: 'm1',
  path: '/media/photos',
  expected: 'label:PHOTOS',
  current: null,
  state: 'offline',
};
const online: Mount = {
  uuid: 'm2',
  path: '/media/backup',
  expected: 'uuid:1234-ABCD',
  current: 'uuid:1234-ABCD',
  state: 'online',
};

describe('offlineMountFor', () => {
  const mounts = [offline, online];

  test('covers the mount point itself and everything below it', () => {
    expect(offlineMountFor(mounts, '/media/photos')).toBe(offline);
    expect(offlineMountFor(mounts, '/media/photos/2024/a.jpg')).toBe(offline);
  });

  test('a mounted volume freezes nothing — those files are there', () => {
    expect(offlineMountFor(mounts, '/media/backup/a.jpg')).toBe(null);
  });

  test('does not match a sibling sharing the prefix, nor an ancestor', () => {
    expect(offlineMountFor(mounts, '/media/photos-backup/a.jpg')).toBe(null);
    expect(offlineMountFor(mounts, '/media')).toBe(null);
    expect(offlineMountFor(mounts, '')).toBe(null);
  });

  test('an unresolvable mount point (null path) matches nothing', () => {
    expect(offlineMountFor([{ ...offline, path: null }], '/media/photos/a.jpg')).toBe(null);
  });
});

describe('relativeTo', () => {
  test('turns an absolute path into the repo-relative form the daemon uses', () => {
    expect(relativeTo('/home/u/repo', '/home/u/repo/media/a.jpg')).toBe('/media/a.jpg');
    expect(relativeTo('/home/u/repo/', '/home/u/repo/media/a.jpg')).toBe('/media/a.jpg');
    expect(relativeTo('/home/u/repo', '/home/u/repo')).toBe('');
  });

  test('rejects a path outside the repository, prefix look-alikes included', () => {
    expect(relativeTo('/home/u/repo', '/home/u/repo2/a.jpg')).toBe(null);
    expect(relativeTo('/home/u/repo', '/elsewhere/a.jpg')).toBe(null);
  });
});

describe('unavailableLabel', () => {
  test('names the volume to plug back in and where it belongs', () => {
    const label = unavailableLabel(offline);
    expect(label).toContain('label:PHOTOS');
    expect(label).toContain('/media/photos');
  });
});

describe('fetchMounts', () => {
  test('reads GET /repos/:repo/mounts', async () => {
    const call = vi.fn(async () => ({ mounts: [offline] }));
    expect(await fetchMounts({ call } as never, 'r1')).toEqual([offline]);
    expect(call).toHaveBeenCalledWith('GET', '/repos/r1/mounts');
  });

  test('a daemon that cannot answer degrades to "no mount points"', async () => {
    const call = vi.fn(async () => {
      throw new Error('daemon down');
    });
    expect(await fetchMounts({ call } as never, 'r1')).toEqual([]);
  });
});

// Video playback position (spec-gui "file panel type" — resume where you
// stopped): the decision of what to persist, and the daemon round-trips that
// store it on the metarecord of the played file.

import { describe, expect, test, vi } from 'vitest';
import {
  SEEK_STEP,
  SEEK_STEP_LONG,
  SPEED_STEPS,
  VOLUME_STEP,
  seekTarget,
  nextSpeed,
  nextVolume,
  PLAYBACK_FIELD,
  MIN_RESUME,
  END_MARGIN,
  MIN_DELTA,
  playbackAction,
  resumeTarget,
  formatPosition,
  loadPosition,
  savePosition,
  clearPosition,
} from '../../default-config/panel-types/file/playback.js';

describe('playbackAction', () => {
  test('saves a position in the middle of the video', () => {
    expect(playbackAction(600, 3600)).toBe('save');
  });

  test('clears near the start: there is nothing worth resuming', () => {
    expect(playbackAction(0, 3600)).toBe('clear');
    expect(playbackAction(MIN_RESUME - 0.1, 3600)).toBe('clear');
    expect(playbackAction(MIN_RESUME + 0.1, 3600)).toBe('save');
  });

  test('clears near the end: the video was watched through', () => {
    expect(playbackAction(3600 - END_MARGIN + 1, 3600)).toBe('clear');
    expect(playbackAction(3600, 3600)).toBe('clear');
    expect(playbackAction(3600 - END_MARGIN - 1, 3600)).toBe('save');
  });

  test('does nothing when the duration is unknown: the end cannot be judged', () => {
    expect(playbackAction(600, NaN)).toBe('none');
    expect(playbackAction(600, Infinity)).toBe('none');
    expect(playbackAction(600, 0)).toBe('none');
  });

  test('does nothing on a nonsensical current time', () => {
    expect(playbackAction(NaN, 3600)).toBe('none');
    expect(playbackAction(-1, 3600)).toBe('none');
  });
});

describe('resumeTarget', () => {
  test('resumes at the stored position', () => {
    expect(resumeTarget(600, 3600)).toBe(600);
  });

  test('ignores a missing or unusable stored position', () => {
    expect(resumeTarget(null, 3600)).toBe(null);
    expect(resumeTarget(NaN, 3600)).toBe(null);
    expect(resumeTarget(-5, 3600)).toBe(null);
    expect(resumeTarget(MIN_RESUME - 0.1, 3600)).toBe(null);
  });

  test('ignores a stored position at or past the end (stale value)', () => {
    expect(resumeTarget(3600, 3600)).toBe(null);
    expect(resumeTarget(3600 - END_MARGIN + 1, 3600)).toBe(null);
    expect(resumeTarget(4000, 3600)).toBe(null);
  });

  test('accepts the position when the duration is not known yet', () => {
    expect(resumeTarget(600, NaN)).toBe(600);
  });
});

describe('formatPosition', () => {
  test('renders mm:ss below an hour, h:mm:ss above', () => {
    expect(formatPosition(0)).toBe('0:00');
    expect(formatPosition(9.7)).toBe('0:09');
    expect(formatPosition(754)).toBe('12:34');
    expect(formatPosition(3723)).toBe('1:02:03');
  });
});

const REPO = 'r1';
const UUID = 'u1';

describe('loadPosition', () => {
  test('reads the float value stored on the metarecord', async () => {
    const daemon = {
      call: vi.fn(async () => ({
        name: PLAYBACK_FIELD,
        values: [{ type: 'float', value: 754.5 }],
      })),
    };
    expect(await loadPosition(daemon, REPO, UUID)).toBe(754.5);
    expect(daemon.call).toHaveBeenCalledWith(
      'GET',
      `/repos/${REPO}/metarecords/${UUID}/fields/${PLAYBACK_FIELD}`,
    );
  });

  test('an unset field, a Nothing value or a non-numeric value reads as null', async () => {
    const empty = { call: vi.fn(async () => ({ name: PLAYBACK_FIELD, values: [] })) };
    expect(await loadPosition(empty, REPO, UUID)).toBe(null);
    const nothing = {
      call: vi.fn(async () => ({ name: PLAYBACK_FIELD, values: [{ type: 'nothing' }] })),
    };
    expect(await loadPosition(nothing, REPO, UUID)).toBe(null);
    const text = {
      call: vi.fn(async () => ({ name: PLAYBACK_FIELD, values: [{ type: 'string', value: 'x' }] })),
    };
    expect(await loadPosition(text, REPO, UUID)).toBe(null);
  });

  test('an int value is accepted (the field may have been set by hand)', async () => {
    const daemon = {
      call: vi.fn(async () => ({ name: PLAYBACK_FIELD, values: [{ type: 'int', value: 60 }] })),
    };
    expect(await loadPosition(daemon, REPO, UUID)).toBe(60);
  });

  test('a failing daemon call is not fatal: no stored position', async () => {
    const daemon = {
      call: vi.fn(async () => {
        throw new Error('boom');
      }),
    };
    expect(await loadPosition(daemon, REPO, UUID)).toBe(null);
  });
});

describe('savePosition / clearPosition', () => {
  test('a save replaces the field with a float value', async () => {
    const daemon = { call: vi.fn(async () => ({})) };
    expect(await savePosition(daemon, REPO, UUID, 754.5)).toBe(754.5);
    expect(daemon.call).toHaveBeenCalledWith(
      'PUT',
      `/repos/${REPO}/metarecords/${UUID}/fields/${PLAYBACK_FIELD}`,
      { value: { type: 'float', value: 754.5 } },
    );
  });

  test('a clear unsets the field', async () => {
    const daemon = { call: vi.fn(async () => ({})) };
    expect(await clearPosition(daemon, REPO, UUID)).toBe(null);
    expect(daemon.call).toHaveBeenCalledWith(
      'DELETE',
      `/repos/${REPO}/metarecords/${UUID}/fields/${PLAYBACK_FIELD}`,
    );
  });

  // Every write goes through the daemon's event log (one revision per write),
  // so a failed write must not be retried in a loop, and must not take the
  // panel down either: playing a video is not a write-critical operation.
  test('a failing write is swallowed', async () => {
    const daemon = {
      call: vi.fn(async () => {
        throw new Error('boom');
      }),
    };
    await expect(savePosition(daemon, REPO, UUID, 12)).resolves.toBe(null);
    await expect(clearPosition(daemon, REPO, UUID)).resolves.toBe(null);
  });
});

describe('write throttling', () => {
  // The panel persists on pause/seek/teardown and periodically while playing.
  // MIN_DELTA guards the periodic path: without it a paused-but-ticking
  // element would write an identical value every interval, one event-log
  // revision each time.
  test('MIN_DELTA is the smallest position change worth a revision', () => {
    expect(MIN_DELTA).toBeGreaterThan(0);
  });
});


// ── Playback controls (spec-gui "file panel type") ──────────────────────
//
// The arithmetic behind the file panel's transport commands, kept pure so the
// clamping is tested without a media element.

describe('seekTarget', () => {
  test('steps forward and back by the given delta', () => {
    expect(seekTarget(100, SEEK_STEP, 3600)).toBe(110);
    expect(seekTarget(100, -SEEK_STEP, 3600)).toBe(90);
    expect(seekTarget(100, SEEK_STEP_LONG, 3600)).toBe(160);
  });

  test('never goes before the start', () => {
    expect(seekTarget(4, -SEEK_STEP, 3600)).toBe(0);
    expect(seekTarget(0, -SEEK_STEP_LONG, 3600)).toBe(0);
  });

  test('never goes past the end', () => {
    expect(seekTarget(3595, SEEK_STEP, 3600)).toBe(3600);
  });

  test('an unknown duration only bounds the start', () => {
    expect(seekTarget(100, SEEK_STEP, NaN)).toBe(110);
    expect(seekTarget(5, -SEEK_STEP, NaN)).toBe(0);
  });

  test('an unreadable position cannot be seeked from', () => {
    expect(seekTarget(NaN, SEEK_STEP, 3600)).toBeNull();
  });
});

describe('nextSpeed', () => {
  test('walks the speed ladder', () => {
    expect(nextSpeed(1, 1)).toBe(1.25);
    expect(nextSpeed(1, -1)).toBe(0.75);
  });

  test('stops at both ends of the ladder', () => {
    const slowest = SPEED_STEPS[0];
    const fastest = SPEED_STEPS[SPEED_STEPS.length - 1];
    expect(nextSpeed(slowest, -1)).toBe(slowest);
    expect(nextSpeed(fastest, 1)).toBe(fastest);
  });

  test('a rate between two steps moves to the next one either way', () => {
    expect(nextSpeed(1.1, 1)).toBe(1.25);
    expect(nextSpeed(1.1, -1)).toBe(1);
  });

  test('an unreadable rate falls back to normal speed', () => {
    expect(nextSpeed(NaN, 1)).toBe(1);
  });
});

describe('nextVolume', () => {
  test('steps up and down', () => {
    expect(nextVolume(0.5, VOLUME_STEP)).toBe(0.6);
    expect(nextVolume(0.5, -VOLUME_STEP)).toBe(0.4);
  });

  test('stays within 0..1', () => {
    expect(nextVolume(0.95, VOLUME_STEP)).toBe(1);
    expect(nextVolume(0.05, -VOLUME_STEP)).toBe(0);
  });

  test('does not drift on repeated steps (floating point)', () => {
    let volume = 0;
    for (let i = 0; i < 10; i += 1) volume = nextVolume(volume, VOLUME_STEP);
    expect(volume).toBe(1);
    expect(nextVolume(0.7, VOLUME_STEP)).toBe(0.8);
  });

  test('an unreadable volume falls back to full', () => {
    expect(nextVolume(NaN, -VOLUME_STEP)).toBe(0.9);
  });
});

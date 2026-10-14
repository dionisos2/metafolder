// The file panel's slideshow (doc "Slideshow"): what `file:play` takes, what
// each kind of file gets, how long a GIF lasts, and the order of the list.

import { describe, expect, test } from 'vitest';
import {
  parsePlayArgs,
  stepFor,
  gifDurationMs,
  createPlaylist,
} from '../../default-config/panel-types/file/slideshow.js';

describe('parsePlayArgs', () => {
  test('a target alone plays in order and stops at the end', () => {
    expect(parsePlayArgs(['query'])).toEqual({ target: 'query', shuffle: false, loop: false });
    expect(parsePlayArgs(['selection'])).toEqual({
      target: 'selection',
      shuffle: false,
      loop: false,
    });
  });

  test('the options come in any order, empty ones ignored', () => {
    expect(parsePlayArgs(['query', 'loop', 'shuffle'])).toEqual({
      target: 'query',
      shuffle: true,
      loop: true,
    });
    expect(parsePlayArgs(['selection', 'shuffle', ''])).toEqual({
      target: 'selection',
      shuffle: true,
      loop: false,
    });
  });

  test('stop takes no option', () => {
    expect(parsePlayArgs(['stop'])).toEqual({ target: 'stop', shuffle: false, loop: false });
    expect(() => parsePlayArgs(['stop', 'loop'])).toThrow(/stop/);
  });

  test('an unknown target or option is refused, naming it', () => {
    expect(() => parsePlayArgs(['all'])).toThrow(/"all"/);
    expect(() => parsePlayArgs([undefined])).toThrow(/selection \/ query \/ stop/);
    expect(() => parsePlayArgs(['query', 'random'])).toThrow(/"random"/);
  });
});

describe('stepFor', () => {
  test('media plays, images and GIFs wait, everything else is skipped', () => {
    expect(stepFor('video')).toBe('media');
    expect(stepFor('audio')).toBe('media');
    expect(stepFor('image')).toBe('image');
    expect(stepFor('gif')).toBe('gif');
    expect(stepFor('document')).toBe('skip');
    expect(stepFor(null)).toBe('skip');
    expect(stepFor(undefined)).toBe('skip');
  });
});

/** A minimal GIF89a: a 1×1 screen with a 2-colour global table, then one
 *  frame per entry of `delays` (centiseconds; null = no Graphic Control
 *  Extension before that frame). */
function gif(delays: (number | null)[], { comment = false } = {}): Uint8Array {
  const bytes: number[] = [];
  bytes.push(...[...'GIF89a'].map((c) => c.charCodeAt(0)));
  bytes.push(1, 0, 1, 0, 0x80, 0, 0); // 1×1, global table of 2 entries
  bytes.push(0, 0, 0, 255, 255, 255);
  if (comment) bytes.push(0x21, 0xfe, 3, 0x61, 0x62, 0x63, 0); // a comment extension
  for (const delay of delays) {
    if (delay !== null) bytes.push(0x21, 0xf9, 4, 0, delay & 0xff, delay >> 8, 0, 0);
    bytes.push(0x2c, 0, 0, 0, 0, 1, 0, 1, 0, 0); // image descriptor, no local table
    bytes.push(2, 2, 0x4c, 0x01, 0); // LZW min size, one sub-block, terminator
  }
  bytes.push(0x3b);
  return new Uint8Array(bytes);
}

describe('gifDurationMs', () => {
  test('sums the delays of the frames', () => {
    expect(gifDurationMs(gif([10, 20, 30]))).toBe(600);
  });

  test('a delay of 0 or 1 is shown as 100 ms, as the browsers do', () => {
    expect(gifDurationMs(gif([0, 1, 5]))).toBe(250);
  });

  test('a frame without a control extension counts as a 0 delay', () => {
    expect(gifDurationMs(gif([null, 50]))).toBe(600);
  });

  test('other extensions are stepped over', () => {
    expect(gifDurationMs(gif([10, 10], { comment: true }))).toBe(200);
  });

  test('delays above 255 centiseconds read both bytes', () => {
    expect(gifDurationMs(gif([300, 300]))).toBe(6000);
  });

  test('a still (one frame) has no duration of its own', () => {
    expect(gifDurationMs(gif([50]))).toBeNull();
  });

  test('a truncated or foreign file has none either', () => {
    const whole = gif([10, 10]);
    expect(gifDurationMs(whole.slice(0, whole.length - 8))).toBeNull();
    expect(gifDurationMs(new Uint8Array([0x89, 0x50, 0x4e, 0x47]))).toBeNull();
    expect(gifDurationMs(new Uint8Array())).toBeNull();
  });
});

/** A deterministic `random`: the values in turn, then 0. */
function randoms(...values: number[]) {
  return () => values.shift() ?? 0;
}

describe('createPlaylist', () => {
  test('plays in order and ends', () => {
    const list = createPlaylist(['a', 'b', 'c'], { shuffle: false, loop: false });
    expect([list.next(), list.next(), list.next(), list.next()]).toEqual(['a', 'b', 'c', null]);
  });

  test('an empty list ends at once, even when looping', () => {
    expect(createPlaylist([], { shuffle: false, loop: true }).next()).toBeNull();
  });

  test('loop starts over', () => {
    const list = createPlaylist(['a', 'b'], { shuffle: false, loop: true });
    expect([list.next(), list.next(), list.next(), list.next()]).toEqual(['a', 'b', 'a', 'b']);
  });

  test('shuffle plays each once, in a random order', () => {
    // Fisher–Yates from the end: i=2 takes j=floor(0*3)=0, i=1 takes j=0.
    const list = createPlaylist(['a', 'b', 'c'], { shuffle: true, loop: false }, randoms(0, 0));
    const played = [list.next(), list.next(), list.next(), list.next()];
    expect(played).toEqual(['b', 'c', 'a', null]);
  });

  test('a looping shuffle draws a new order each pass, never repeating across the seam', () => {
    // Pass 1: identity (0.99 keeps each element in place). Pass 2 draws
    // c, b, a (i=2 takes 0, i=1 keeps 1), which would play 'c' twice in a row:
    // the seam swaps it away.
    const list = createPlaylist(
      ['a', 'b', 'c'],
      { shuffle: true, loop: true },
      randoms(0.99, 0.99, 0, 0.5),
    );
    const played = Array.from({ length: 6 }, () => list.next());
    expect(played.slice(0, 3)).toEqual(['a', 'b', 'c']);
    expect(played[3]).not.toBe('c');
    expect([...played.slice(3)].sort()).toEqual(['a', 'b', 'c']);
  });

  test('jumpTo continues from a file the user chose', () => {
    const list = createPlaylist(['a', 'b', 'c', 'd'], { shuffle: false, loop: false });
    expect(list.next()).toBe('a');
    expect(list.jumpTo('c')).toBe(true);
    expect(list.next()).toBe('d');
    expect(list.jumpTo('z')).toBe(false); // not in the list: the place is kept
    expect(list.next()).toBeNull();
  });

  test('size is the number of files listed', () => {
    expect(createPlaylist(['a', 'b'], { shuffle: true, loop: false }).size).toBe(2);
  });
});

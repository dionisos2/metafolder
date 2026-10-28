// The slideshow's model (doc "Slideshow"): the arguments of `viewer:play`, what
// each kind of file gets, how long a GIF lasts, and the order the files come
// in. Pure, so the panel's part is only timers and elements.

// What the durations fall back to when `[panel-defaults.viewer]` has none.
export const IMAGE_SECONDS = 3;
// A GIF whose length cannot be read (a still, a truncated file).
export const GIF_SECONDS = 5;
// The pause after an audio or a video has played to its end.
export const GAP_SECONDS = 0.5;

const TARGETS = ['selection', 'query', 'stop'];
export const PLAY_OPTIONS = ['shuffle', 'loop'];

/**
 * What `viewer:play <target> [option] [option]` asks for. The options are a set:
 * they come in any order, and a slot left empty is no option.
 *
 * @param {(string|undefined)[]} args
 * @returns {{target: 'selection'|'query'|'stop', shuffle: boolean, loop: boolean}}
 */
export function parsePlayArgs(args) {
  const [target, ...rest] = args;
  if (!target || !TARGETS.includes(target)) {
    throw new Error(`unknown target: "${target ?? ''}" (expected ${TARGETS.join(' / ')})`);
  }
  const options = rest.filter((o) => o !== undefined && o !== '');
  for (const option of options) {
    if (!PLAY_OPTIONS.includes(/** @type {string} */ (option))) {
      throw new Error(`unknown option: "${option}" (expected ${PLAY_OPTIONS.join(' / ')})`);
    }
  }
  if (target === 'stop' && options.length > 0) throw new Error('stop takes no option');
  return {
    target: /** @type {'selection'|'query'|'stop'} */ (target),
    shuffle: options.includes('shuffle'),
    loop: options.includes('loop'),
  };
}

/**
 * What the slideshow does with a file of this kind (the content's kind, as
 * `fs.stat` reads it): play it to its end, show it for a while, or move on.
 *
 * @param {string|null|undefined} kind
 * @returns {'media'|'image'|'gif'|'skip'}
 */
export function stepFor(kind) {
  if (kind === 'video' || kind === 'audio') return 'media';
  if (kind === 'image') return 'image';
  if (kind === 'gif') return 'gif';
  return 'skip';
}

// A frame delay at or below this many centiseconds is drawn at 100 ms by
// every browser (WebKit included): a GIF saying "0" does not flash by.
const MIN_GIF_DELAY_CS = 1;
const DEFAULT_GIF_DELAY_MS = 100;

/**
 * How long one pass of an animated GIF lasts, in milliseconds: the sum of its
 * frames' delays, read from their Graphic Control Extensions. Null when there
 * is nothing to play through — a single frame — or when the bytes are not a
 * whole GIF; the caller then waits a fixed time instead.
 *
 * @param {Uint8Array} bytes the whole file
 * @returns {number|null}
 */
export function gifDurationMs(bytes) {
  const header = String.fromCharCode(...bytes.subarray(0, 6));
  if (header !== 'GIF87a' && header !== 'GIF89a') return null;
  let at = 13;
  if (bytes.length < at) return null;
  const screen = bytes[10];
  if (screen & 0x80) at += 3 * (1 << ((screen & 0x07) + 1)); // global colour table

  /** Steps over a run of data sub-blocks; the index after its terminator, or
   *  -1 when the bytes end first. @param {number} from */
  const skipSubBlocks = (from) => {
    let i = from;
    while (i < bytes.length) {
      const size = bytes[i];
      if (size === 0) return i + 1;
      i += size + 1;
    }
    return -1;
  };

  let frames = 0;
  let total = 0;
  let delay = 0; // the delay the next frame takes, from the extension before it
  while (at < bytes.length) {
    const block = bytes[at];
    if (block === 0x3b) {
      // The trailer: the file is whole.
      return frames >= 2 ? total : null;
    }
    if (block === 0x21) {
      if (at + 1 >= bytes.length) return null;
      if (bytes[at + 1] === 0xf9) {
        if (at + 5 >= bytes.length) return null;
        delay = bytes[at + 4] | (bytes[at + 5] << 8);
      }
      at = skipSubBlocks(at + 2);
    } else if (block === 0x2c) {
      if (at + 10 > bytes.length) return null;
      const packed = bytes[at + 9];
      at += 10;
      if (packed & 0x80) at += 3 * (1 << ((packed & 0x07) + 1)); // local colour table
      at = skipSubBlocks(at + 1); // past the LZW minimum code size
      frames += 1;
      total += delay <= MIN_GIF_DELAY_CS ? DEFAULT_GIF_DELAY_MS : delay * 10;
      delay = 0;
    } else {
      return null;
    }
    if (at < 0) return null;
  }
  return null; // no trailer: truncated
}

/**
 * The files of a slideshow, in the order they are shown. `next()` gives the
 * next uuid, or null once the list is over — never, with `loop`, unless it is
 * empty. A shuffled loop draws a new order for each pass.
 *
 * @param {string[]} uuids
 * @param {{shuffle: boolean, loop: boolean}} options
 * @param {() => number} [random] in [0, 1), for the tests
 */
export function createPlaylist(uuids, { shuffle, loop }, random = Math.random) {
  /** @param {string[]} items */
  const shuffled = (items) => {
    const out = [...items];
    for (let i = out.length - 1; i > 0; i -= 1) {
      const j = Math.floor(random() * (i + 1));
      [out[i], out[j]] = [out[j], out[i]];
    }
    return out;
  };
  let order = shuffle ? shuffled(uuids) : [...uuids];
  let position = -1;

  return {
    size: uuids.length,
    /** @returns {string|null} */
    next() {
      if (order.length === 0) return null;
      if (position + 1 < order.length) {
        position += 1;
        return order[position];
      }
      if (!loop) return null;
      const last = order[order.length - 1];
      if (shuffle) {
        order = shuffled(uuids);
        // A new pass must not start with the file the last one ended on.
        if (order.length > 1 && order[0] === last) [order[0], order[1]] = [order[1], order[0]];
      }
      position = 0;
      return order[0];
    },
    /** Continues from `uuid` (a file the user chose): the next one is the one
     *  after it. False, and the place kept, when it is not in the list.
     *  @param {string} uuid */
    jumpTo(uuid) {
      const index = order.indexOf(uuid);
      if (index < 0) return false;
      position = index;
      return true;
    },
  };
}

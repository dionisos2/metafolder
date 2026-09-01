// Video playback position (spec-gui "file panel type"): where the user
// stopped watching, stored as an ordinary field on the metarecord of the
// played file, so reopening the video resumes there.
//
// The field is a plain user field: `mfr_*` is reserved (writes need `force`)
// and an unknown `mf_*` name is rejected outright by the daemon.
export const PLAYBACK_FIELD = 'playback_position';

/**
 * The one API method these helpers need — spelled out so a test can supply a
 * stub without building the whole `metafolder.daemon`.
 * @typedef {Pick<Metafolder.Daemon, 'call'>} Daemon
 */

// Below this, the video has barely started: there is nothing to resume to.
export const MIN_RESUME = 5;
// Within this of the end, the video counts as watched through: resuming would
// drop the viewer straight back onto the credits. An absolute margin, not a
// percentage — 5% of a three-hour film is nine minutes, which is not "the end".
export const END_MARGIN = 10;
// Smallest position change worth writing. Every write is one event-log
// revision, so the periodic save skips a position that has not really moved.
export const MIN_DELTA = 1;
// How often the position is persisted while the video plays. Pause, seek and
// teardown persist immediately; this only bounds what an abruptly killed
// window can lose.
export const SAVE_INTERVAL_MS = 15000;

// What to persist for a video sitting at `currentTime` of `duration`:
// 'save' the position, 'clear' any stored one (start/end), or do 'none' of
// it because the numbers cannot be judged (a stream with no known duration —
// storing a position we could never recognize as stale).
/**
 * @param {number} currentTime @param {number} duration
 * @returns {'save'|'clear'|'none'}
 */
export function playbackAction(currentTime, duration) {
  if (!Number.isFinite(currentTime) || currentTime < 0) return 'none';
  if (!Number.isFinite(duration) || duration <= 0) return 'none';
  if (currentTime < MIN_RESUME) return 'clear';
  if (currentTime > duration - END_MARGIN) return 'clear';
  return 'save';
}

// The time to seek to on open, or null to start from the beginning. `duration`
// may be unknown (NaN) — the metadata has not loaded yet — in which case the
// stored position is taken at face value.
/**
 * @param {number|null} saved @param {number} duration NaN when still unknown
 * @returns {number|null}
 */
export function resumeTarget(saved, duration) {
  if (saved === null || !Number.isFinite(saved) || saved < MIN_RESUME) return null;
  if (Number.isFinite(duration) && duration > 0 && saved > duration - END_MARGIN) return null;
  return saved;
}

// "12:34" / "1:02:03", as a media player shows it.
/** @param {number} seconds */
export function formatPosition(seconds) {
  const total = Math.max(0, Math.floor(seconds));
  const s = total % 60;
  const m = Math.floor(total / 60) % 60;
  const h = Math.floor(total / 3600);
  const pad = (/** @type {number} */ n) => String(n).padStart(2, '0');
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

/** @param {string} repo @param {string} uuid */
function fieldUrl(repo, uuid) {
  return `/repos/${repo}/metarecords/${uuid}/fields/${PLAYBACK_FIELD}`;
}

// The stored position of a metarecord, or null when there is none. A field
// holding anything but a number (Nothing, or a value a user typed by hand)
// is treated as "no position" rather than an error: the preview must play.
/**
 * @param {Daemon} daemon @param {string} repo @param {string} uuid
 * @returns {Promise<number|null>}
 */
export async function loadPosition(daemon, repo, uuid) {
  /** @type {{values?: Metafolder.Value[]}|undefined} */
  let response;
  try {
    response = /** @type {{values?: Metafolder.Value[]}} */ (
      await daemon.call('GET', fieldUrl(repo, uuid))
    );
  } catch {
    return null; // unreachable daemon / unknown metarecord: just play from the start
  }
  const value = response?.values?.[0];
  if (!value || (value.type !== 'float' && value.type !== 'int')) return null;
  return Number.isFinite(value.value) ? value.value : null;
}

// Store the position (replacing any previous one). Returns what is now stored,
// so the caller can track it without a re-read. A write failure is swallowed:
// watching a video must not fail because the daemon refused a field write.
/**
 * @param {Daemon} daemon @param {string} repo @param {string} uuid
 * @param {number} seconds
 * @returns {Promise<number|null>}
 */
export async function savePosition(daemon, repo, uuid, seconds) {
  try {
    await daemon.call('PUT', fieldUrl(repo, uuid), {
      value: { type: 'float', value: seconds },
    });
    return seconds;
  } catch {
    return null;
  }
}

// Remove the stored position. Only call this when one is actually stored: an
// unset of an absent field would still open an event-log revision.
/**
 * @param {Daemon} daemon @param {string} repo @param {string} uuid
 * @returns {Promise<null>}
 */
export async function clearPosition(daemon, repo, uuid) {
  try {
    await daemon.call('DELETE', fieldUrl(repo, uuid));
  } catch {
    // Already gone / unreachable: nothing to undo.
  }
  return null;
}

// ── Playback controls ───────────────────────────────────────────────────
//
// The arithmetic behind the panel's transport commands (play/pause, seek,
// speed, volume). Kept here, pure and clamped, so the commands themselves are
// one line each and the edge cases are tested without a media element. A media
// element reports NaN for `currentTime`/`duration` before its metadata is in,
// so every helper must survive one.

// Seek steps: the short one for nudging past a scene, the long one for
// skipping an opening or finding your way back into a film.
export const SEEK_STEP = 10;
export const SEEK_STEP_LONG = 60;
// The playback rates the speed commands walk through (mpv's ladder around 1×).
export const SPEED_STEPS = [0.25, 0.5, 0.75, 1, 1.25, 1.5, 1.75, 2, 3, 4];
export const VOLUME_STEP = 0.1;

// Slack for comparing a rate against the ladder: a rate read back from a media
// element is not always bit-identical to the value that was written.
const SPEED_EPSILON = 1e-6;

/**
 * Where a seek of `delta` seconds from `currentTime` lands: inside the media,
 * never before its start nor past its end. `duration` may be NaN (metadata not
 * in yet), which only drops the upper bound. Null when the position itself
 * cannot be read — there is nothing to seek from.
 *
 * @param {number} currentTime @param {number} delta
 * @param {number} duration NaN when still unknown
 * @returns {number|null}
 */
export function seekTarget(currentTime, delta, duration) {
  if (!Number.isFinite(currentTime)) return null;
  const target = Math.max(0, currentTime + delta);
  if (!Number.isFinite(duration) || duration <= 0) return target;
  return Math.min(target, duration);
}

/**
 * The next rate up (`direction` 1) or down (-1) the ladder, stopping at its
 * ends. A rate that is not on the ladder (set elsewhere) moves to the nearest
 * step in that direction; an unreadable one falls back to normal speed.
 *
 * @param {number} rate @param {1|-1} direction
 * @returns {number}
 */
export function nextSpeed(rate, direction) {
  if (!Number.isFinite(rate) || rate <= 0) return 1;
  if (direction > 0) {
    return SPEED_STEPS.find((step) => step > rate + SPEED_EPSILON) ?? SPEED_STEPS.at(-1) ?? 1;
  }
  const slower = SPEED_STEPS.filter((step) => step < rate - SPEED_EPSILON);
  return slower.at(-1) ?? SPEED_STEPS[0];
}

/**
 * The volume `delta` away from `volume`, clamped to 0..1 and rounded to the
 * step's precision — repeated steps must land exactly on 1, not on
 * 0.9999999999999999.
 *
 * @param {number} volume @param {number} delta
 * @returns {number}
 */
export function nextVolume(volume, delta) {
  const from = Number.isFinite(volume) ? volume : 1;
  return Math.min(1, Math.max(0, Math.round((from + delta) * 100) / 100));
}

// Video playback position (spec-gui "file panel type"): where the user
// stopped watching, stored as an ordinary field on the metarecord of the
// played file, so reopening the video resumes there.
//
// The field is a plain user field: `mfr_*` is reserved (writes need `force`)
// and an unknown `mf_*` name is rejected outright by the daemon.
export const PLAYBACK_FIELD = 'playback_position';

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
export function resumeTarget(saved, duration) {
  if (saved === null || !Number.isFinite(saved) || saved < MIN_RESUME) return null;
  if (Number.isFinite(duration) && duration > 0 && saved > duration - END_MARGIN) return null;
  return saved;
}

// "12:34" / "1:02:03", as a media player shows it.
export function formatPosition(seconds) {
  const total = Math.max(0, Math.floor(seconds));
  const s = total % 60;
  const m = Math.floor(total / 60) % 60;
  const h = Math.floor(total / 3600);
  const pad = (n) => String(n).padStart(2, '0');
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

function fieldUrl(repo, uuid) {
  return `/repos/${repo}/metarecords/${uuid}/fields/${PLAYBACK_FIELD}`;
}

// The stored position of a metarecord, or null when there is none. A field
// holding anything but a number (Nothing, or a value a user typed by hand)
// is treated as "no position" rather than an error: the preview must play.
export async function loadPosition(daemon, repo, uuid) {
  let response;
  try {
    response = await daemon.call('GET', fieldUrl(repo, uuid));
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
export async function clearPosition(daemon, repo, uuid) {
  try {
    await daemon.call('DELETE', fieldUrl(repo, uuid));
  } catch {
    // Already gone / unreachable: nothing to undo.
  }
  return null;
}

// What may be edited in the viewer panel, and how it goes back to the disk
// (doc "Editing a text file"). Saving replaces the whole file with what the field
// holds, so a file is editable only when that field holds it exactly.

/**
 * The text to edit in `bytes`, or why there is none.
 *
 * @param {Uint8Array} bytes what was read of the file
 * @param {boolean} truncated the file goes on past `bytes`
 * @returns {{ok: true, text: string, eol: '\n'|'\r\n'} | {ok: false, reason: string}}
 */
export function editableText(bytes, truncated) {
  if (truncated) return { ok: false, reason: 'too large to edit (text-preview-limit)' };
  if (bytes.includes(0)) return { ok: false, reason: 'not editable: it holds NUL bytes' };
  let decoded;
  try {
    // `ignoreBOM` keeps a byte order mark in the text, so it is written back.
    decoded = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true }).decode(bytes);
  } catch {
    return { ok: false, reason: 'not editable: it is not UTF-8' };
  }
  // A text field only knows LF: it reads any other line end as one. CRLF
  // everywhere can be restored on saving; a mix could not.
  const carriageReturns = decoded.split('\r').length - 1;
  if (carriageReturns === 0) return { ok: true, text: decoded, eol: '\n' };
  const unix = decoded.replaceAll('\r\n', '\n');
  if (unix.includes('\r') || unix.split('\n').length - 1 !== carriageReturns) {
    return { ok: false, reason: 'not editable: it mixes line endings' };
  }
  return { ok: true, text: unix, eol: '\r\n' };
}

/**
 * What is written for the text a field holds.
 * @param {string} text @param {'\n'|'\r\n'} eol
 */
export function encodeEdited(text, eol) {
  return eol === '\n' ? text : text.replaceAll('\n', eol);
}

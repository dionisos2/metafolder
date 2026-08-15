// Filesystem-operation helpers for the file-manager panel (spec-gui
// "file-manager panel type"): building destination paths and picking a
// collision-free name for the new-folder/new-file, copy/cut/paste, duplicate
// and rename actions. Pure functions, kept out of main.js so they are unit
// tested (see tests/file-manager-fileops.test.ts).

/**
 * Absolute path of `name` inside directory `dir` (the filesystem root does not
 * double its slash).
 * @param {string} dir @param {string} name
 */
export function joinPath(dir, name) {
  return dir === '/' ? `/${name}` : `${dir}/${name}`;
}

/**
 * Splits a filename into its stem and its last extension (including the dot).
 * A leading dot is a hidden-file marker, not an extension, and a name with no
 * interior dot has an empty extension.
 * @param {string} name @returns {[string, string]}
 */
export function splitExt(name) {
  const dot = name.lastIndexOf('.');
  // dot <= 0 covers "no dot" and a leading-dot hidden name; a trailing dot
  // (dot === length-1) is meaningless as an extension, so keep it on the stem.
  if (dot <= 0 || dot === name.length - 1) return [name, ''];
  return [name.slice(0, dot), name.slice(dot)];
}

/**
 * A name for `name` that does not collide with anything in `taken`. A free name
 * is returned unchanged; otherwise " copy" is inserted before the extension
 * (numbered " copy 2", " copy 3"… on further collisions), mirroring a desktop
 * file manager.
 * @param {string} name @param {Set<string>} taken @returns {string}
 */
export function dedupeName(name, taken) {
  if (!taken.has(name)) return name;
  const [stem, ext] = splitExt(name);
  let candidate = `${stem} copy${ext}`;
  let n = 2;
  while (taken.has(candidate)) {
    candidate = `${stem} copy ${n}${ext}`;
    n += 1;
  }
  return candidate;
}

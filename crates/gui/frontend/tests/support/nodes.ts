// DOM identity, not structure: `toEqual` compares nodes by shape, so a list
// rebuilt with the same content would pass for the one it replaced.

/** Whether `after` holds exactly the node objects of `before`, in order. */
export function sameNodes(after: Element[], before: Element[]): boolean {
  return after.length === before.length && after.every((node, i) => node === before[i]);
}

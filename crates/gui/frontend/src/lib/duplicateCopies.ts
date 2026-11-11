// `mf:duplicate keep|trash query|selection` (doc "Deleting duplicate copies"):
// the GUI half of `mf duplicate keep` / `mf duplicate trash`. The copies to
// trash are themselves a query, each with a twin the same run leaves alone, so
// no group loses its last copy — the query guarantees it, not a check here.

import { formatSize, pathBytes, pathOfBytes } from "../../../panel-shim/ui.js";
import { invoke } from "./ipc";
import { status } from "./status";
import { isWirePath, wireText, type WireText } from "./treePath";

export type CopiesMode = "keep" | "trash";

const GROUP = "mfr_duplicate_group";
const PAGE = 500;

/** The query selecting the copies to trash, given the user's query `q`:
 *  `keep` trashes the copies outside `q` in the groups with one inside it,
 *  `trash` the copies inside `q` in the groups with one outside it. */
export function copiesToTrash(mode: CopiesMode, q: unknown): unknown {
  const not = (operand: unknown) => ({ type: "not", operand });
  const same = (target: unknown) => ({ type: "same_as", field: GROUP, target });
  return mode === "keep"
    ? { type: "and", operands: [same(q), not(q)] }
    : { type: "and", operands: [q, same(not(q))] };
}

/** A daemon round-trip, throwing the daemon's error on >= 400. */
async function daemon<T>(method: string, path: string, body: unknown): Promise<T> {
  const res = await invoke<{ status: number; body: unknown }>("daemon_request", {
    method,
    path,
    body,
  });
  if (res.status >= 400) {
    const error = (res.body as { error?: string } | null)?.error;
    throw new Error(error ?? `daemon ${method} ${path} failed (HTTP ${res.status})`);
  }
  return res.body as T;
}

type Field = { name: string; value: { value?: unknown } };
type Found = { uuid: string; fields?: Field[] };
type Page = { results?: Found[]; next_cursor?: string | null };

const fieldOf = (rec: Found, name: string) =>
  rec.fields?.find((f) => f.name === name)?.value?.value;

/** What the user's query is: the list's, or the checked metarecords. `null`
 *  after saying why there is none — an empty selection never falls back on the
 *  query (doc "Editing metarecords in the GUI"). */
async function userQuery(ws: string, mode: CopiesMode, source: string): Promise<unknown> {
  const verb = mode === "keep" ? "keep" : "trash";
  if (source === "query") {
    const q = await invoke<unknown>("ws_get_var", {
      wsId: ws,
      key: "metarecord-list:effective-query",
    });
    if (!q) await status(`The metarecord list shows no query — no copy to ${verb}.`, "info");
    return q || null;
  }
  if (source === "selection") {
    const checked = await invoke<unknown>("ws_get_var", { wsId: ws, key: "selected_metarecords" });
    const uuids = Array.isArray(checked) ? checked.filter((u) => typeof u === "string") : [];
    if (uuids.length === 0) {
      await status(`Nothing is checked — no copy to ${verb}.`, "info");
      return null;
    }
    return { type: "uuid_in", uuids };
  }
  await status(`unknown source: "${source}" (expected query or selection)`);
  return null;
}

/** `root` joined with the repository path `rel`, as their exact bytes. */
function underRoot(root: string, rel: WireText): WireText {
  const a = pathBytes(root);
  const b = pathBytes(rel);
  const joined = new Uint8Array(a.length + b.length);
  joined.set(a);
  joined.set(b, a.length);
  return pathOfBytes(joined);
}

/** Lists the copies to trash, asks, and trashes them one by one; a failure
 *  stops the run, the copies already trashed having left their groups. */
export async function trashCopies(
  ws: string,
  repo: string,
  mode: CopiesMode,
  source: string,
): Promise<void> {
  const q = await userQuery(ws, mode, source);
  if (q === null) return;
  const victims = copiesToTrash(mode, q);
  try {
    const found: Found[] = [];
    let cursor: string | null = null;
    do {
      const page: Page = await daemon<Page>(
        "POST",
        `/repos/${repo}/query`,
        {
          query: victims,
          select: [GROUP, "mfr_size", "mfr_inode"],
          limit: PAGE,
          ...(cursor ? { cursor } : {}),
        },
      );
      found.push(...(page.results ?? []));
      cursor = page.next_cursor ?? null;
    } while (cursor);
    if (found.length === 0) {
      await status("No duplicate copy to trash", "info");
      return;
    }
    const paths = await daemon<Record<string, unknown>>(
      "POST",
      `/repos/${repo}/query/fields/resolve-tree`,
      { query: victims },
    );
    const repos = await daemon<{ repo_uuid: string; root: string }[]>("GET", "/repos", null);
    const root = repos.find((r) => r.repo_uuid === repo)?.root;
    if (!root) throw new Error("the repository root is unknown");

    const groups = new Set<string>();
    let reclaimable = 0;
    const copies: { rel: WireText; text: string }[] = [];
    for (const rec of found) {
      const rel = (paths[rec.uuid] as unknown[] | undefined)?.find(isWirePath);
      if (rel === undefined) continue;
      groups.add(String(fieldOf(rec, GROUP)));
      // A name sharing its inode frees nothing once trashed.
      if (fieldOf(rec, "mfr_inode") === undefined) reclaimable += Number(fieldOf(rec, "mfr_size") ?? 0);
      copies.push({ rel, text: wireText(rel) ?? "" });
    }
    copies.sort((a, b) => (a.text < b.text ? -1 : a.text > b.text ? 1 : 0));

    const n = copies.length;
    const g = groups.size;
    const plural = (k: number) => (k === 1 ? "copy" : "copies");
    const question =
      `Send ${n} duplicate ${plural(n)}, in ${g} group${g === 1 ? "" : "s"}, to the trash?\n\n` +
      copies.map((c) => `  ${c.text}`).join("\n") +
      `\n\nUp to ${formatSize(reclaimable)} is reclaimed once the trash is pruned.`;
    if (!window.confirm(question)) return;

    let trashed = 0;
    try {
      for (const c of copies) {
        await invoke("trash_path", { repo, path: underRoot(root, c.rel) });
        trashed += 1;
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      throw new Error(`${message} (after trashing ${trashed} of ${n})`, { cause: error });
    } finally {
      if (trashed > 0)
        await invoke("ws_set_var", { wsId: ws, key: "metarecords:dirty", value: Date.now() });
    }
    await status(`Trashed ${trashed} duplicate ${plural(trashed)}`, "info");
  } catch (error) {
    await status(error instanceof Error ? error.message : String(error));
  }
}

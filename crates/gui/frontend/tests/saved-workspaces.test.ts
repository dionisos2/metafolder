// The saved-workspace commands (doc "Saved workspaces"): each asks what it
// needs with the repository's saved names as completions, `workspace:save`
// confirms before replacing a name that exists, and the work itself is the
// Rust side's (`saved_workspace_*`).

import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { dispatch } from "../src/lib/commands";
import { store } from "../src/lib/store.svelte";

const { invoked } = vi.hoisted(() => ({
  invoked: [] as { cmd: string; args: unknown }[],
}));

vi.mock("../src/lib/ipc", () => ({
  invoke: vi.fn(async (cmd: string, args: unknown) => {
    invoked.push({ cmd, args });
    if (cmd === "saved_workspace_list")
      return [
        { name: "tri", kind: "work", default: true },
        { name: "vue", kind: "layout", default: false },
      ];
    if (cmd === "saved_workspace_open") return "ws-1";
    if (
      cmd === "saved_workspace_unset_default" ||
      cmd === "saved_workspace_delete"
    )
      return true;
    return null;
  }),
  listen: vi.fn(async () => () => {}),
}));

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

async function answer(value: string | null): Promise<void> {
  await settle();
  const resolve = store.ui.promptResolver;
  expect(store.ui.promptText).not.toBeNull();
  store.ui.promptText = null;
  store.ui.promptResolver = null;
  resolve?.(value);
  await settle();
}

const calls = (cmd: string) =>
  invoked.filter((c) => c.cmd === cmd).map((c) => c.args);

beforeEach(() => {
  invoked.length = 0;
  store.workspaces = [
    { id: "ws-1", name: "photos 1", active_repo: "r1", repo_name: "photos" },
  ];
  store.layout.left = {
    visible: true,
    workspace_id: "ws-1",
    panel_type: "metarecord-list",
  };
});

afterEach(() => {
  store.layout.left = { visible: false, workspace_id: null, panel_type: null };
  store.workspaces = [];
  vi.restoreAllMocks();
});

describe("workspace:save", () => {
  test("asks the kind, then the name over the saved ones, and saves a new name at once", async () => {
    const confirm = vi.spyOn(window, "confirm");
    const done = dispatch("workspace:save");
    await settle();
    expect(store.ui.promptText).toBe("Save what? (layout / work)");
    expect(store.ui.promptCompletions).toEqual(["layout", "work"]);
    await answer("work");
    expect(store.ui.promptCompletions).toEqual(["tri", "vue"]);
    await answer("nouveau");
    expect(await done).toEqual({ ok: true });
    expect(confirm).not.toHaveBeenCalled();
    expect(calls("saved_workspace_save")).toEqual([
      { wsId: "ws-1", kind: "work", name: "nouveau" },
    ]);
  });

  test("a name that exists is replaced only once confirmed", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    expect(await dispatch("workspace:save layout vue")).toEqual({ ok: true });
    expect(confirm).toHaveBeenCalledOnce();
    expect(confirm.mock.calls[0][0]).toContain('"vue"');
    expect(calls("saved_workspace_save")).toEqual([]);

    confirm.mockReturnValue(true);
    expect(await dispatch("workspace:save layout vue")).toEqual({ ok: true });
    expect(calls("saved_workspace_save")).toEqual([
      { wsId: "ws-1", kind: "layout", name: "vue" },
    ]);
  });

  test("a name of several words needs no quoting", async () => {
    vi.spyOn(window, "confirm");
    await dispatch("workspace:save work tri des photos");
    expect(calls("saved_workspace_save")).toEqual([
      { wsId: "ws-1", kind: "work", name: "tri des photos" },
    ]);
  });
});

describe("workspace:open", () => {
  test("asks here or new, then the name over the saved ones", async () => {
    const done = dispatch("workspace:open");
    await settle();
    expect(store.ui.promptText).toBe("Open where? (here / new)");
    expect(store.ui.promptCompletions).toEqual(["here", "new"]);
    await answer("new");
    expect(store.ui.promptCompletions).toEqual(["tri", "vue"]);
    await answer("tri");
    expect(await done).toEqual({ ok: true });
    expect(calls("saved_workspace_open")).toEqual([
      { wsId: "ws-1", target: "new", name: "tri" },
    ]);
  });
});

describe("the default and deleting", () => {
  test("set-default asks the name, unset-default asks nothing", async () => {
    const done = dispatch("workspace:set-default");
    await settle();
    expect(store.ui.promptCompletions).toEqual(["tri", "vue"]);
    await answer("vue");
    expect(await done).toEqual({ ok: true });
    expect(calls("saved_workspace_set_default")).toEqual([
      { wsId: "ws-1", name: "vue" },
    ]);

    expect(await dispatch("workspace:unset-default")).toEqual({ ok: true });
    expect(calls("saved_workspace_unset_default")).toEqual([{ wsId: "ws-1" }]);
  });

  test("delete-saved deletes the name given", async () => {
    expect(await dispatch("workspace:delete-saved tri")).toEqual({ ok: true });
    expect(calls("saved_workspace_delete")).toEqual([
      { wsId: "ws-1", name: "tri" },
    ]);
  });
});

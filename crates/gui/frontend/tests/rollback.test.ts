// `log:rollback <time>` (doc "Rollback"): the plan is read, the user
// confirms how much goes back, then the revert is run.

import { beforeEach, describe, expect, test, vi } from "vitest";
import { dispatch } from "../src/lib/commands";

const { tauri, plan } = vi.hoisted(
  (): {
    tauri: { cmd: string; args: unknown }[];
    plan: { value: unknown };
  } => ({
    tauri: [],
    plan: { value: {} },
  }),
);

vi.mock("../src/lib/ipc", () => ({
  invoke: vi.fn(async (cmd: string, args: unknown) => {
    tauri.push({ cmd, args });
    return cmd === "log_rollback_plan" ? plan.value : null;
  }),
  listen: vi.fn(async () => () => {}),
}));

const calls = (cmd: string) =>
  tauri.filter((c) => c.cmd === cmd).map((c) => c.args);

describe("log:rollback", () => {
  beforeEach(() => {
    tauri.length = 0;
    plan.value = { since: 5, operations: 3, changed: 1, files: false };
  });

  test("confirmed, it goes back to the time the plan read", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    await dispatch("log:rollback 2h", { ws: "ws-1", slot: "left" });
    expect(calls("log_rollback_plan")).toEqual([{ wsId: "ws-1", time: "2h" }]);
    expect(confirm.mock.calls[0]?.[0]).toContain("3 operation(s)");
    expect(confirm.mock.calls[0]?.[0]).toContain(
      "1 of them overwriting a later change",
    );
    expect(calls("log_rollback")).toEqual([{ wsId: "ws-1", since: 5 }]);
  });

  test("declined, nothing is reverted", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(false);
    await dispatch("log:rollback 2h", { ws: "ws-1", slot: "left" });
    expect(calls("log_rollback")).toEqual([]);
  });

  test("with nothing since the time, nothing is asked", async () => {
    plan.value = { since: 5, operations: 0, changed: 0, files: false };
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    confirm.mockClear();
    await dispatch("log:rollback 2h", { ws: "ws-1", slot: "left" });
    expect(confirm).not.toHaveBeenCalled();
    expect(calls("log_rollback")).toEqual([]);
  });
});

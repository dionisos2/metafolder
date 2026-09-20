// Where a panel command's declared arguments live (spec-gui "Command").
//
// A panel type is mounted once per workspace, and every instance registers the
// same command names. The argument functions (`prompt`, `initial`, `complete`,
// `when`) close over the state of the instance that declared them, so a
// registry keyed by name alone hands the focused workspace whatever instance
// mounted last — its completions against another workspace's handler. Here the
// specs are keyed by instance and answered for the *focused* workspace, the
// same one `setPanelDispatch` runs the handler on.

import type { ArgSpec, PanelArgSource } from '../commands';

export interface PanelArgSourceDeps {
  /** The workspace whose panels answer right now (null: none focused). */
  focusedWs: () => string | null;
  /** The panel type owning a command, or undefined for a shell builtin. */
  ownerOf: (name: string) => string | undefined;
  /** Mounts (and awaits) one workspace's instance of a panel type. */
  ensureMounted: (wsId: string, panelType: string) => Promise<void>;
}

/** A `PanelArgSource` over a per-instance store, plus the two calls PanelHost
 *  drives it with: `register` as an instance declares a command, `forget` as
 *  the instance is torn down (the key is the instance's, `<wsId>|<panelType>`,
 *  the same one its handlers use). */
export interface PanelArgRegistry extends PanelArgSource {
  register(instanceKey: string, name: string, args: ArgSpec[]): void;
  forget(instanceKey: string): void;
}

export function createPanelArgSource(deps: PanelArgSourceDeps): PanelArgRegistry {
  /** `<wsId>|<panelType>|<name>` → that instance's declared arguments. */
  const specs = new Map<string, ArgSpec[]>();

  return {
    register(instanceKey, name, args) {
      specs.set(`${instanceKey}|${name}`, args);
    },

    forget(instanceKey) {
      for (const key of [...specs.keys()]) {
        if (key.startsWith(`${instanceKey}|`)) specs.delete(key);
      }
    },

    // A workspace that has never displayed the owning panel has no instance of
    // it, and so no spec: mount it first, or the command would run with the
    // arguments it was invoked with and never ask for the rest.
    async prepare(name) {
      const wsId = deps.focusedWs();
      const owner = deps.ownerOf(name);
      if (!wsId || !owner) return; // a shell builtin, or nothing focused
      await deps.ensureMounted(wsId, owner);
    },

    // Command names are unique across panel types (the registry is keyed by
    // name), so the panel type need not be known to find the spec.
    resolve(name) {
      const wsId = deps.focusedWs();
      if (!wsId) return undefined;
      for (const [key, args] of specs) {
        if (key.startsWith(`${wsId}|`) && key.endsWith(`|${name}`)) return args;
      }
      return undefined;
    },
  };
}

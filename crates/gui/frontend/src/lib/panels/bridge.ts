// Shell side of the shell⇄panel postMessage protocol. The pure core
// (createBridgeCore) is unit tested; PanelHost owns the DOM (iframes,
// positioning) and wires window.onmessage to it.
//
// Instances are keyed by a string id, NOT by Window identity: WebKitGTK
// swaps the iframe's WindowProxy on cross-origin navigation, so a
// contentWindow captured at creation never matches event.source later.
// PanelHost resolves event.source against the live iframes instead.

export interface PanelMeta {
  wsId: string;
  panelType: string;
}

interface Instance {
  meta: PanelMeta;
  post: (message: Record<string, unknown>) => void;
  subscriptions: Set<string>;
}

export interface BridgeDeps {
  invoke: (command: string, args?: Record<string, unknown>) => Promise<unknown>;
  dispatch: (invocation: string) => Promise<void>;
  onCommandsChanged: () => void;
}

export function createBridgeCore(deps: BridgeDeps) {
  const instances = new Map<string, Instance>();
  let nextInvocationId = 1;
  const pendingCommands = new Map<number, { resolve: () => void; reject: (e: unknown) => void }>();

  async function handleRequest(
    instance: Instance,
    method: string,
    params: Record<string, unknown>,
  ): Promise<unknown> {
    const { wsId, panelType } = instance.meta;
    switch (method) {
      case 'daemon.request':
        return deps.invoke('daemon_request', {
          method: params.method,
          path: params.path,
          body: params.body ?? null,
        });
      case 'daemon.parseQuery':
        return deps.invoke('parse_query', { dsl: params.dsl });
      case 'workspace.get':
        return deps.invoke('ws_get_var', { wsId, key: params.key });
      case 'workspace.set':
        return deps.invoke('ws_set_var', { wsId, key: params.key, value: params.value });
      case 'workspace.adoptRepo':
        return deps.invoke('adopt_repo', { wsId, repo: params.repo });
      case 'workspace.subscribe':
        instance.subscriptions.add(String(params.key));
        return null;
      case 'commands.register':
        await deps.invoke('register_command', {
          panelType,
          name: params.name,
          label: params.label ?? String(params.name),
          scope: params.scope ?? null,
          reveal: params.reveal ?? false,
        });
        deps.onCommandsChanged();
        return null;
      case 'commands.invoke':
        await deps.dispatch(String(params.invocation));
        return null;
      case 'addKeybinding':
        return deps.invoke('suggest_keybinding', {
          combo: params.combo,
          invocation: params.invocation,
          when: params.when === undefined ? panelType : params.when,
          textInput: params.textInput ?? false,
        });
      case 'statusBar.message':
        return deps.invoke('post_status', {
          wsId,
          text: params.text,
          kind: params.kind ?? 'info',
          timeoutMs: params.timeoutMs ?? null,
        });
      case 'messages.list':
        return deps.invoke('get_messages', { wsId });
      case 'fs.readDir':
        return deps.invoke('fs_read_dir', { path: params.path });
      case 'fs.stat':
        return deps.invoke('fs_stat', { path: params.path });
      default:
        throw `unknown metafolder API method: ${method}`;
    }
  }

  return {
    register(source: string, meta: PanelMeta, post: Instance['post']) {
      instances.set(source, { meta, post, subscriptions: new Set() });
    },

    unregister(source: string) {
      instances.delete(source);
    },

    instanceMeta(source: string): PanelMeta | undefined {
      return instances.get(source)?.meta;
    },

    async onMessage(source: string, data: unknown) {
      const instance = instances.get(source);
      const message = data as Record<string, unknown> | null;
      if (!instance || !message || message.mf !== true) return;

      switch (message.type) {
        case 'request': {
          const id = message.id;
          try {
            const result = await handleRequest(
              instance,
              String(message.method),
              (message.params ?? {}) as Record<string, unknown>,
            );
            instance.post({ mf: true, type: 'response', id, ok: true, result });
          } catch (error) {
            instance.post({ mf: true, type: 'response', id, ok: false, error: String(error) });
          }
          break;
        }
        case 'key-resolved':
          await deps.dispatch(String(message.invocation));
          break;
        case 'command-result': {
          const pending = pendingCommands.get(Number(message.invocationId));
          if (pending) {
            pendingCommands.delete(Number(message.invocationId));
            if (message.ok) pending.resolve();
            else pending.reject(message.error ?? 'panel command failed');
          }
          break;
        }
      }
    },

    /** Pushes a workspace variable change to subscribed panels. */
    forwardVarChange(wsId: string, key: string, value: unknown) {
      for (const instance of instances.values()) {
        if (instance.meta.wsId !== wsId) continue;
        if (instance.subscriptions.has(key) || instance.subscriptions.has('*')) {
          instance.post({ mf: true, type: 'var-changed', key, value });
        }
      }
    },

    /** Pushes a message-log append (message panel type). */
    forwardMessageAppended(wsId: string, entry: unknown) {
      for (const instance of instances.values()) {
        if (instance.meta.wsId !== wsId) continue;
        if (instance.subscriptions.has('messages')) {
          instance.post({ mf: true, type: 'message-appended', entry });
        }
      }
    },

    /** Pushes the recompiled keybinding table to every panel. */
    pushKeytable(bindings: unknown) {
      for (const instance of instances.values()) {
        instance.post({ mf: true, type: 'keytable', bindings });
      }
    },

    pushVisibility(source: string, visible: boolean, slot: string | null) {
      instances.get(source)?.post({ mf: true, type: 'visibility', visible, slot });
    },

    /** Sends a registered command to its panel; resolves on command-result. */
    dispatchCommand(source: string, name: string, args: string[]): Promise<void> {
      const instance = instances.get(source);
      if (!instance) return Promise.reject('panel not available');
      const invocationId = nextInvocationId++;
      return new Promise((resolve, reject) => {
        pendingCommands.set(invocationId, { resolve, reject });
        instance.post({ mf: true, type: 'command', invocationId, name, args });
      });
    },
  };
}

export type BridgeCore = ReturnType<typeof createBridgeCore>;

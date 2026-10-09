import { createPluginStorage, type PluginStorage } from "./storage";
import * as React from "react";
import { PluginRuntime } from "./runtime";
import type { Context } from "@deepseek-ai/cordis";
import type { PluginManifest, PluginModule } from "./api";
import { createModuleLoader } from "./modules";
import { withTimeout } from "./timeout";
import type {
  StorageResult,
  ConfigurationState,
  ManagementAction,
} from "./types";

export type BundledPlugin = {
  manifest: PluginManifest;
  module: PluginModule;
  enabledByDefault?: boolean;
};

// One configuration observer drives activation, whether or not Settings is mounted.
export function createPluginManager(
  ctx: Context,
  {
    bundled,
    storage = createPluginStorage(() =>
      bundled.map(({ manifest, enabledByDefault = true }) => ({
        manifest,
        source: "bundled",
        enabled: enabledByDefault,
        revision: "bundled",
        previous: null,
        reloadable: false,
        error: null,
      })),
    ),
  }: {
    bundled: readonly BundledPlugin[];
    storage?: PluginStorage | undefined;
  },
) {
  ctx.provide("react", React);
  const runtime = new PluginRuntime(
    ctx,
    createModuleLoader(
      Object.fromEntries(
        bundled.map(({ manifest, module }) => [manifest.id, module]),
      ),
      storage.readModule,
    ),
  );
  let configuration: ConfigurationState = { status: "loading" };
  let busy = false;
  let catalogPending = false;
  let error: string | null = null;
  let refreshError: string | null = null;
  let closed = false;
  let changes = 0;
  let timer: ReturnType<typeof setTimeout>;
  const listeners = new Set<() => void>();
  const read = () => ({
    configuration,
    activation: runtime.snapshot(),
    busy,
    error,
    refreshError,
  });
  let snapshot = read();
  const publish = () => {
    if (closed) return;
    snapshot = read();
    for (const listener of listeners) listener();
  };
  const unsubscribe = runtime.subscribe(publish);

  function accept(next: StorageResult) {
    if (JSON.stringify(configuration) === JSON.stringify(next) && !refreshError)
      return;
    configuration = next;
    refreshError = null;
    const desired =
      next.status === "ready"
        ? next.catalog.plugins.filter(
            (plugin) =>
              plugin.enabled &&
              !(next.externalPluginsPaused && plugin.source === "external"),
          )
        : [];
    runtime.reconcile(desired);
    publish();
  }
  async function getCatalog() {
    if (catalogPending)
      throw new Error("Plugin catalog read is still in progress");
    catalogPending = true;
    try {
      return await storage.getCatalog();
    } finally {
      catalogPending = false;
    }
  }
  async function refresh() {
    const before = changes;
    try {
      if (busy || catalogPending) return;
      const next = await withTimeout(
        getCatalog(),
        "Plugin storage did not respond within 10 seconds",
      );
      if (!closed && before === changes) accept(next);
    } catch (reason) {
      if (!closed && before === changes) {
        if (configuration.status === "ready") {
          refreshError = String(reason);
          publish();
        } else
          accept({
            status: "recovery",
            reason: String(reason),
            canReset: false,
          });
      }
    } finally {
      if (!closed) timer = setTimeout(() => void refresh(), 1000);
    }
  }
  // Runs one management write. Rejects when another write is in progress or
  // the write fails; the caller reports the failure.
  async function perform(
    operation: () => Promise<StorageResult>,
    timeoutMs = 10_000,
  ) {
    if (closed) throw new Error("Plugin management has stopped");
    if (busy) throw new Error("Another plugin change is in progress");
    busy = true;
    changes++;
    publish();
    // A watchdog ends the caller's wait, not the underlying native operation.
    let settled = false;
    const pending = (async () => {
      try {
        return await operation();
      } finally {
        settled = true;
      }
    })();
    const finish = () => {
      changes++;
      busy = false;
      publish();
    };
    try {
      const next = await withTimeout(
        pending,
        `Plugin storage did not respond within ${timeoutMs / 1000} seconds`,
        timeoutMs,
      );
      if (closed) throw new Error("Plugin management has stopped");
      accept(next);
    } finally {
      if (settled) finish();
      else void pending.then(finish, finish);
    }
  }
  // Settings writes report failure in the shared error banner.
  async function update(
    operation: () => Promise<StorageResult>,
    timeoutMs = 10_000,
  ) {
    if (busy || closed) return false;
    error = null;
    try {
      await perform(operation, timeoutMs);
      return true;
    } catch (reason) {
      if (!closed) {
        error = String(reason);
        publish();
      }
      return false;
    }
  }
  void refresh();
  return {
    snapshot: () => snapshot,
    startup: () => configuration.status,
    subscribe(listener: () => void) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    imports: storage.imports,
    perform,
    /** Storage writes for `perform`; they do not update the catalog alone. */
    changePlugin: storage.changePlugin,
    installImport: (token: string, path: string) =>
      update(() => {
        if (!storage.imports)
          throw new Error("Plugin imports require the desktop app");
        return storage.imports.install(token, path);
      }),
    change: (action: ManagementAction, id: string) =>
      update(() => storage.changePlugin(action, id)),
    reload: (id: string) => update(() => storage.reloadPlugin(id), 120_000),
    retry: () => update(getCatalog),
    recover: () => update(storage.recoverSettings),
    dismissError: () => {
      error = null;
      publish();
    },
    async dispose() {
      closed = true;
      clearTimeout(timer);
      unsubscribe();
      listeners.clear();
      await runtime.dispose();
    },
  };
}
export type PluginManager = ReturnType<typeof createPluginManager>;

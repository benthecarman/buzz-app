import { Context } from "@deepseek-ai/cordis";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { createPluginManager, type PluginManager } from "../../plugins/manager";
import type { PluginManifest } from "../../plugins/api";
import type { PluginStorage } from "../../plugins/storage";
import type {
  ImportPreview,
  PluginInfo,
  StorageResult,
} from "../../plugins/types";
import { HostService, type HostPlugins } from "./service";

const managing: PluginManifest = {
  id: "example.manager",
  name: "Manager",
  apiVersion: 1,
  host: { plugins: true },
};
const plain: PluginManifest = {
  id: "example.plain",
  name: "Plain",
  apiVersion: 1,
};
const target: PluginManifest = {
  id: "example.target",
  name: "Target",
  apiVersion: 1,
};
const info = (manifest: PluginManifest, enabled = true): PluginInfo => ({
  manifest,
  source: "bundled",
  enabled,
  revision: "bundled",
  previous: null,
  reloadable: false,
  error: null,
});
const ready = (
  targetEnabled = true,
  plugins = [info(managing), info(plain), info(target, targetEnabled)],
): StorageResult => ({
  status: "ready",
  externalPluginsPaused: false,
  catalog: { profile: "test", location: "test", plugins },
});
const preview: ImportPreview = {
  token: "token",
  source: "https://example.test/repo.git",
  commit: "abc",
  candidates: [{ path: "", manifest: target, revision: "r1" }],
  warnings: [],
};

const managers: PluginManager[] = [];
const roots: Context[] = [];
beforeEach(() => vi.useFakeTimers());
afterEach(async () => {
  await Promise.all(managers.splice(0).map((manager) => manager.dispose()));
  await Promise.all(roots.splice(0).map((root) => root.fiber.dispose()));
  vi.useRealTimers();
});

async function harness(overrides: Partial<PluginStorage> = {}) {
  const imports = {
    folder: vi.fn(async () => null),
    git: vi.fn(async () => preview),
    install: vi.fn(async () => ready()),
    discard: vi.fn(async () => {}),
  };
  const storage: PluginStorage = {
    getCatalog: vi.fn(async () => ready()),
    changePlugin: vi.fn(async () => ready(false)),
    recoverSettings: vi.fn(async () => ready()),
    readModule: vi.fn(async () => ""),
    reloadPlugin: vi.fn(async () => ready()),
    imports,
    ...overrides,
  };
  const root = new Context();
  roots.push(root);
  const granted: Record<string, HostPlugins | undefined> = {};
  const module = (id: string) => ({
    inject: ["host"],
    apply(ctx: Context) {
      granted[id] = ctx.host.plugins;
    },
  });
  const plugins = createPluginManager(root, {
    storage,
    bundled: [
      { manifest: managing, module: module(managing.id) },
      { manifest: plain, module: module(plain.id) },
      { manifest: target, module: { apply() {} } },
    ],
  });
  managers.push(plugins);
  new HostService(root, plugins);
  await vi.advanceTimersByTimeAsync(0);
  const host = (id: string) => {
    const plugins = granted[id];
    if (!plugins) throw new Error(`${id} did not activate`);
    return plugins;
  };
  return { plugins, storage, imports, host };
}

it("lets a plugin that declares host.plugins read and change the catalog", async () => {
  const { plugins, storage, host: grant } = await harness();
  const host = grant(managing.id);
  expect(host.snapshot()).toEqual(ready());
  const changed = vi.fn();
  host.subscribe(changed);
  await host.change("disable", target.id);
  expect(storage.changePlugin).toHaveBeenCalledWith("disable", target.id);
  // The write resolves after the shared catalog shows its result.
  expect(host.snapshot()).toEqual(ready(false));
  expect(plugins.snapshot().configuration).toEqual(ready(false));
  expect(changed).toHaveBeenCalled();
});

it("refuses a plugin that does not declare host.plugins", async () => {
  const { storage, imports, host: grant } = await harness();
  const host = grant(plain.id);
  expect(() => host.snapshot()).toThrow("does not declare host.plugins");
  expect(() => host.subscribe(() => {})).toThrow(
    "does not declare host.plugins",
  );
  await expect(host.change("disable", target.id)).rejects.toThrow(
    "does not declare host.plugins",
  );
  await expect(host.importGit("https://example.test/repo.git")).rejects.toThrow(
    "does not declare host.plugins",
  );
  await expect(host.install("token", "")).rejects.toThrow(
    "does not declare host.plugins",
  );
  expect(storage.changePlugin).not.toHaveBeenCalled();
  expect(imports.git).not.toHaveBeenCalled();
  expect(imports.install).not.toHaveBeenCalled();
});

it("refuses callers without a plugin installation", async () => {
  const { plugins } = await harness();
  const root = new Context();
  roots.push(root);
  new HostService(root, plugins);
  expect(() => root.host.plugins?.snapshot()).toThrow(
    "needs an installed plugin",
  );
});

it("imports and installs through the shared manager", async () => {
  const {
    plugins,
    imports,
    host: grant,
  } = await harness({
    changePlugin: vi.fn(async () => ready()),
  });
  const host = grant(managing.id);
  expect(
    await host.importGit("https://example.test/repo.git", "main", "nip98"),
  ).toBe(preview);
  expect(imports.git).toHaveBeenCalledWith(
    "https://example.test/repo.git",
    "main",
    "nip98",
  );
  await host.install("token", "");
  expect(imports.install).toHaveBeenCalledWith("token", "");
  await host.discardImport("token");
  expect(imports.discard).toHaveBeenCalledWith("token");
  expect(plugins.snapshot().busy).toBe(false);
});

it("rejects a failed or overlapping write without touching the Settings error", async () => {
  let release!: (result: StorageResult) => void;
  const {
    plugins,
    storage,
    host: grant,
  } = await harness({
    changePlugin: vi
      .fn<PluginStorage["changePlugin"]>()
      .mockImplementationOnce(() => new Promise((done) => (release = done)))
      .mockRejectedValueOnce("Channels is required and cannot be disabled"),
  });
  const host = grant(managing.id);
  const first = host.change("disable", target.id);
  await expect(host.change("enable", target.id)).rejects.toThrow(
    "Another plugin change is in progress",
  );
  // Settings sees the same write as busy.
  expect(await plugins.change("enable", target.id)).toBe(false);
  release(ready(false));
  await first;
  const failure = host.change("disable", "buzz.channels");
  await expect(failure).rejects.toBeInstanceOf(Error);
  await expect(failure).rejects.toThrow("Channels is required");
  expect(plugins.snapshot().error).toBeNull();
  expect(storage.changePlugin).toHaveBeenCalledTimes(2);
  await expect(host.change("rollback" as "enable", target.id)).rejects.toThrow(
    "Unknown plugin action",
  );
});

it("stops granting once the plugin is turned off", async () => {
  const { host: grant, plugins } = await harness({
    changePlugin: vi.fn(async () =>
      ready(true, [info(managing, false), info(plain), info(target)]),
    ),
  });
  const host = grant(managing.id);
  await plugins.change("disable", managing.id);
  expect(() => host.snapshot()).toThrow("does not declare host.plugins");
});

it("stops granting once a new revision drops host.plugins", async () => {
  const { host: grant, plugins } = await harness({
    changePlugin: vi.fn(async () =>
      ready(true, [
        {
          ...info(managing),
          manifest: { id: managing.id, name: managing.name, apiVersion: 1 },
          source: "external",
          revision: "r2",
        },
        info(plain),
        info(target),
      ]),
    ),
  });
  const host = grant(managing.id);
  expect(host.snapshot().status).toBe("ready");
  await plugins.change("enable", managing.id);
  expect(() => host.snapshot()).toThrow("does not declare host.plugins");
  await expect(host.change("disable", target.id)).rejects.toThrow(
    "does not declare host.plugins",
  );
});

it("reports that imports need the desktop app in the browser", async () => {
  const { host: grant } = await harness({ imports: undefined });
  const host = grant(managing.id);
  await expect(host.importGit("https://example.test/repo.git")).rejects.toThrow(
    "require the desktop app",
  );
  await expect(host.install("token", "")).rejects.toThrow(
    "require the desktop app",
  );
});

import { test, expect } from "vitest";
import { mkdtemp, writeFile, symlink, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

test("generated author package in a path with spaces exposes agentControl, host, identity names, and page navigation metadata", async () => {
  const root = fileURLToPath(new URL("../../", import.meta.url));
  const dir = await mkdtemp(join(tmpdir(), "buzz-author-consumer-"));
  const env = {
    ...process.env,
    PATH: `${resolve(root, "bin")}:${process.env.PATH}`,
  };
  const run = (command, args) => {
    const result = spawnSync(command, args, {
      cwd: root,
      env,
      encoding: "utf8",
      timeout: 60_000,
    });
    expect(result.error).toBeUndefined();
    expect(result.status, `${result.stdout}\n${result.stderr}`).toBe(0);
  };
  try {
    await symlink(
      resolve(root, "node_modules"),
      join(dir, "node_modules"),
      "dir",
    );
    run("node", ["scripts/build-author.mjs", join(dir, "author package")]);
    await writeFile(
      join(dir, "consumer.ts"),
      `
import type { Context, AgentControl, Host, HostPlugins, PluginInfo, PluginManifest, NamingPolicy } from "@buzz/author";
export const manifest: PluginManifest = {
  id: "example.plugin", name: "Example", apiVersion: 1,
  host: {
    commands: [{ id: "status", program: "example-cli", args: ["status"], maxOutputBytes: 65536 }],
    networkOrigins: ["https://api.example.com"],
    plugins: true,
  },
};
export const inject = ["agentControl", "host", "identityNames", "pages"];
export function apply(ctx: Context) {
  const policy: NamingPolicy = {
    id: "alternative",
    resolve: (identities) => new Map(identities.map(({ pubkey, name }) => [pubkey, { name }])),
  };
  ctx.identityNames.register(policy);
  const control: AgentControl = ctx.agentControl;
  void control.refresh();
  void control.action("sample", "stop");
  const host: Host = ctx.host;
  void host.runCommand("status").then((output: string | null) => void output);
  void host.request({
    url: "https://api.example.com/graphql",
    method: "POST",
    headers: { Authorization: "Bearer sample" },
    body: "{}",
  }).then((response) => void response.status);
  const plugins: HostPlugins | undefined = host.plugins;
  if (plugins) {
    const installed: PluginInfo[] = plugins.snapshot().catalog.plugins;
    void installed;
    void plugins.importGit("https://example.test/repo.git").then((preview) =>
      plugins.install(preview.token, preview.candidates[0]?.path ?? ""),
    );
    void plugins.change("enable", "example.other");
    // @ts-expect-error Rollback stays in Settings.
    void plugins.change("rollback", "example.other");
  }
  ctx.pages.register({
    id: "main",
    title: "Example",
    component: () => null,
    icon: "data:image/png;base64,iVBOR",
    primary: true,
    placement: "toolbar",
  });
  ctx.pages.register({
    id: "other",
    title: "Other",
    component: () => null,
    // @ts-expect-error A page icon is a data URL string.
    icon: 1,
  });
  for (const placement of ["sidebar", "topbar", "toolbar"] as const)
    ctx.pages.register({ id: placement, title: placement, component: () => null, primary: true, placement });
  ctx.pages.register({
    id: "bad-placement", title: "Bad placement", component: () => null,
    // @ts-expect-error A placement must name a supported navigation location.
    placement: "floating",
  });
  // @ts-expect-error Native process lifetime is not plugin-owned.
  control.dispose();
  // @ts-expect-error Programs must be declared in the manifest, not supplied at runtime.
  host.runCommand("example-cli", ["status"]);
}
// @ts-expect-error Host construction is not exported to plugin authors.
import type { provideAgentControl } from "@buzz/author";
`,
    );
    await writeFile(
      join(dir, "tsconfig.json"),
      JSON.stringify({
        compilerOptions: {
          target: "ES2022",
          module: "ESNext",
          moduleResolution: "Bundler",
          strict: true,
          skipLibCheck: true,
          noEmit: true,
          types: [],
          paths: {
            "@buzz/author": ["./author package/types/plugins/author.d.ts"],
          },
        },
        files: ["consumer.ts"],
      }),
    );
    // Only generated declarations and the external consumer, never host source.
    run("pnpm", ["exec", "tsc", "-p", join(dir, "tsconfig.json")]);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}, 120_000);

// @vitest-environment jsdom
import "@testing-library/jest-dom/vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import type { PluginManager } from "../plugins/manager";
import type { Catalog, ImportPreview } from "../plugins/types";
import { PluginImport } from "./PluginImport";

afterEach(cleanup);

it("shows exact declared access and changes before an enabled update", async () => {
  const preview: ImportPreview = {
    token: "preview",
    source: "/example",
    commit: null,
    warnings: [],
    candidates: [
      {
        path: "dist",
        revision: "two",
        manifest: {
          id: "example.plugin",
          name: "Example",
          apiVersion: 1,
          host: {
            commands: [
              {
                id: "check",
                program: "example-cli",
                args: ["status", "--json"],
              },
            ],
            networkOrigins: ["https://api.example.com"],
            plugins: true,
          },
        },
      },
    ],
  };
  const catalog: Catalog = {
    profile: "test",
    location: "test",
    plugins: [
      {
        manifest: {
          id: "example.plugin",
          name: "Example",
          apiVersion: 1,
          host: {
            commands: [
              {
                id: "check",
                program: "example-cli",
                args: ["status", "--brief"],
              },
            ],
            networkOrigins: ["https://old.example.com"],
          },
        },
        source: "external",
        enabled: true,
        revision: "one",
        previous: null,
        reloadable: false,
        error: null,
      },
    ],
  };
  const manager = {
    imports: {
      folder: vi.fn(async () => preview),
      git: vi.fn(),
      install: vi.fn(),
      discard: vi.fn(async () => {}),
    },
    installImport: vi.fn(async () => true),
  } as unknown as PluginManager;
  render(<PluginImport plugins={manager} catalog={catalog} busy={false} />);
  fireEvent.click(screen.getByRole("button", { name: "Load from folder" }));
  await waitFor(() =>
    expect(
      screen.getByText(
        /Command check:.*example-cli.*status.*--json.*new or changed/,
      ),
    ).toBeVisible(),
  );
  expect(
    screen.getByText(/HTTPS origin: https:\/\/api.example.com.*new or changed/),
  ).toBeVisible();
  expect(
    screen.getByText(/Manage plugins: install, turn on or off.*new or changed/),
  ).toBeVisible();
  expect(
    screen.getByText(/Removed:.*--brief.*https:\/\/old.example.com/),
  ).toBeVisible();
  expect(
    screen.getByText(
      /stays on and may run immediately unless this launch is in safe mode/,
    ),
  ).toBeVisible();
  expect(manager.installImport).not.toHaveBeenCalled();
});

it.each([
  { previous: undefined, next: 65536, changed: true },
  { previous: 65536, next: 131072, changed: true },
  { previous: 4096, next: 1048576, changed: true },
  { previous: 65536, next: 4096, changed: true },
  { previous: 65536, next: undefined, changed: true },
  { previous: undefined, next: 4096, changed: false },
  { previous: 4096, next: undefined, changed: false },
  { previous: 65536, next: 65536, changed: false },
])(
  "reviews output-limit-only updates from $previous to $next",
  async ({ previous, next, changed }) => {
    const command = {
      id: "tools",
      program: "agent-tools",
      args: ["list", "--json"],
    };
    const manifest = {
      id: "example.plugin",
      name: "Example",
      apiVersion: 1,
    } as const;
    const preview: ImportPreview = {
      token: "preview",
      source: "/example",
      commit: null,
      warnings: [],
      candidates: [
        {
          path: "dist",
          revision: "two",
          manifest: {
            ...manifest,
            host: {
              commands: [
                {
                  ...command,
                  ...(next === undefined ? {} : { maxOutputBytes: next }),
                },
              ],
            },
          },
        },
      ],
    };
    const catalog: Catalog = {
      profile: "test",
      location: "test",
      plugins: [
        {
          manifest: {
            ...manifest,
            host: {
              commands: [
                {
                  ...command,
                  ...(previous === undefined
                    ? {}
                    : { maxOutputBytes: previous }),
                },
              ],
            },
          },
          source: "external",
          enabled: true,
          revision: "one",
          previous: null,
          reloadable: false,
          error: null,
        },
      ],
    };
    const manager = {
      imports: {
        folder: vi.fn(async () => preview),
        discard: vi.fn(async () => {}),
      },
      installImport: vi.fn(),
    } as unknown as PluginManager;
    render(<PluginImport plugins={manager} catalog={catalog} busy={false} />);
    fireEvent.click(screen.getByRole("button", { name: "Load from folder" }));
    const access = within(
      await screen.findByRole("region", { name: "Declared host access" }),
    );
    const grant = access.getByRole("listitem");
    expect(grant).toHaveTextContent(`output: up to ${next ?? 4096} bytes`);
    if (changed) {
      expect(grant).toHaveTextContent("(new or changed)");
      expect(access.getByText(/^Removed:/)).toHaveTextContent(
        `output: up to ${previous ?? 4096} bytes`,
      );
    } else {
      expect(grant).not.toHaveTextContent("(new or changed)");
      expect(access.queryByText(/^Removed:/)).not.toBeInTheDocument();
    }
    expect(manager.installImport).not.toHaveBeenCalled();
  },
);

it("signs in only to repositories the community authorizes", async () => {
  const preview: ImportPreview = {
    token: "preview",
    source: "repo",
    commit: null,
    warnings: [],
    candidates: [],
  };
  const git = vi.fn(async () => preview);
  const manager = {
    imports: {
      folder: vi.fn(),
      git,
      install: vi.fn(),
      discard: vi.fn(async () => {}),
    },
    installImport: vi.fn(),
  } as unknown as PluginManager;
  const buzz = `https://relay.test/git/${"a".repeat(64)}/plugins`;
  const authorizeGit = vi.fn(async (repository: string) =>
    repository.includes("/git/")
      ? { repository: buzz, token: "dG9rZW4=" }
      : null,
  );
  render(
    <PluginImport
      plugins={manager}
      catalog={{ profile: "test", location: "test", plugins: [] }}
      busy={false}
      authorizeGit={authorizeGit}
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: "Load from Git" }));
  const field = screen.getByLabelText("Git or GitHub repository");
  const find = screen.getByRole("button", { name: "Find plugins" });
  fireEvent.change(field, { target: { value: ` ${buzz} ` } });
  fireEvent.click(find);
  await waitFor(() => expect(git).toHaveBeenCalledTimes(1));
  expect(git).toHaveBeenLastCalledWith(buzz, "", "dG9rZW4=");
  fireEvent.change(field, { target: { value: "block/plugins" } });
  await waitFor(() => expect(find).toBeEnabled());
  fireEvent.click(find);
  await waitFor(() => expect(git).toHaveBeenCalledTimes(2));
  expect(git).toHaveBeenLastCalledWith("block/plugins", "");
});

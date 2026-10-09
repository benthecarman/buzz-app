// @vitest-environment jsdom
import "@testing-library/jest-dom/vitest";
import { stubAvatarBrowserApis } from "../../features/agents/avatar-testing";
stubAvatarBrowserApis();
import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import type {
  Agent,
  Agents2,
  AgentsSnapshot,
  RegisteredAgentType,
} from "../../features/agents2/service";
import type { RelayData } from "../../features/relay/service";
import { Agents2Page } from "./Agents2Page";

afterEach(cleanup);

const agent = (name: string, n: number): Agent => ({
  pubkey: String(n).repeat(64),
  name,
  type: "test/echo",
  owner: "f".repeat(64),
  relay: "wss://relay.example",
  config: { reply: `${name} says hi` },
  skipped: {},
  timers: {},
  attentionEnabled: true,
  attention: {
    "interest/default": {
      slug: "interest/default",
      modifiedAt: 1,
      value: { type: "interest", instructions: "Help." },
    },
    "watch/hourly": {
      slug: "watch/hourly",
      modifiedAt: 1,
      value: {
        type: "timer",
        interest_id: "default",
        prompt: "Check in",
        enabled: true,
        interval_secs: 3600,
        armed_at: 1,
        max_occurrences: null,
        expires_at: null,
      },
    },
  },
});

function setup(agents: Agent[], { shown = true } = {}) {
  let snapshot: AgentsSnapshot = { status: "ready", agents };
  const listeners = new Set<() => void>();
  const type = {
    key: "test/echo",
    pluginId: "test",
    revision: "1",
    id: "echo",
    title: "Echo",
    description: "Says its line.",
    defaults: () => ({ config: { reply: "" } }),
    summary: (item: Agent) => (item.config as { reply: string }).reply,
    Peek: ({ agent }: { agent: Agent }) => <p>Peek of {agent.name}</p>,
    tabs: [{ id: "reply", title: "Reply", component: () => <p>Reply tab</p> }],
  } as unknown as RegisteredAgentType;
  const created = agent("Nova", 3);
  const types = [type];
  const agents2 = {
    types: () => types,
    snapshot: () => snapshot,
    subscribe: (listener: () => void) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    find: (pubkey: string) => snapshot.agents.find((a) => a.pubkey === pubkey),
    register: vi.fn(),
    create: vi.fn(async () => {
      // `shown: false`: it was made for a community no longer selected.
      if (!shown) return created;
      snapshot = { ...snapshot, agents: [...snapshot.agents, created] };
      for (const listener of listeners) listener();
      return created;
    }),
    save: vi.fn(async () => {}),
    remove: vi.fn(async () => {}),
  } satisfies Agents2;
  const disconnected = { status: "disconnected", generation: 0 };
  const relay = {
    snapshot: () => disconnected,
    subscribe: () => () => {},
  } as unknown as RelayData;
  render(<Agents2Page agents2={agents2} relay={relay} />);
  return { agents2 };
}

it("peeks at a selected agent beside the grid and follows the arrow keys", async () => {
  const user = userEvent.setup();
  setup([agent("Ada", 1), agent("Bo", 2)]);
  const grid = screen.getByRole("listbox", { name: "Agents" });
  expect(within(grid).getByText("Ada says hi")).toBeInTheDocument();
  expect(screen.queryByText("Peek of Ada")).not.toBeInTheDocument();

  await user.click(within(grid).getByText("Ada"));
  expect(screen.getByRole("option", { name: /Ada/ })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  expect(screen.getByText("Peek of Ada")).toBeInTheDocument();
  expect(screen.getByText("Every hour")).toBeInTheDocument();
  expect(screen.getByText("Mentions and replies")).toBeInTheDocument();

  await user.keyboard("{ArrowRight}");
  expect(screen.getByText("Peek of Bo")).toBeInTheDocument();
  expect(screen.getByRole("option", { name: /Bo/ })).toHaveFocus();

  await user.keyboard("{Escape}");
  expect(screen.queryByText("Peek of Bo")).not.toBeInTheDocument();
  expect(grid).toBeInTheDocument();
});

it("opens the build view with the type's tabs and the app's Attention tab", async () => {
  const user = userEvent.setup();
  setup([agent("Ada", 1)]);
  await user.click(screen.getByRole("option", { name: /Ada/ }));
  await user.keyboard("{Enter}");
  expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
  const tabs = screen.getAllByRole("tab").map((tab) => tab.textContent);
  expect(tabs).toEqual(["Reply", "Attention"]);
  expect(screen.getByText("Reply tab")).toBeInTheDocument();
  await user.click(screen.getByRole("tab", { name: "Attention" }));
  expect(screen.getByText("Default")).toBeInTheDocument();

  await user.click(screen.getByRole("button", { name: "All agents" }));
  expect(screen.getByRole("option", { name: /Ada/ })).toHaveAttribute(
    "aria-selected",
    "true",
  );
});

it("makes a new agent in the side panel and goes straight to building it", async () => {
  const user = userEvent.setup();
  const { agents2 } = setup([agent("Ada", 1)]);
  await user.click(screen.getByRole("button", { name: "New agent" }));
  const form = screen.getByRole("form", { name: "New agent" });
  expect(within(form).getByText("Says its line.")).toBeInTheDocument();
  await user.type(within(form).getByLabelText("Name"), "Nova{Enter}");
  expect(agents2.create).toHaveBeenCalledWith({
    type: "test/echo",
    name: "Nova",
  });
  expect(await screen.findByRole("tab", { name: "Attention" })).toBeVisible();
  expect(screen.getByRole("heading", { name: "Nova" })).toBeInTheDocument();
});

it("stays on the grid when the new agent belongs to a community no longer shown", async () => {
  const user = userEvent.setup();
  const { agents2 } = setup([agent("Ada", 1)], { shown: false });
  await user.click(screen.getByRole("button", { name: "New agent" }));
  const form = screen.getByRole("form", { name: "New agent" });
  await user.type(within(form).getByLabelText("Name"), "Nova{Enter}");
  await vi.waitFor(() => expect(agents2.create).toHaveBeenCalled());
  expect(
    await screen.findByRole("listbox", { name: "Agents" }),
  ).toBeInTheDocument();
  expect(screen.queryByRole("form", { name: "New agent" })).toBeNull();
  expect(screen.queryByRole("tab", { name: "Attention" })).toBeNull();
});

it("saves a rename on Enter and discards it on Escape", async () => {
  const user = userEvent.setup();
  const { agents2 } = setup([agent("Ada", 1)]);
  await user.click(screen.getByRole("option", { name: /Ada/ }));
  await user.keyboard("{Enter}");
  const field = within(
    screen.getByRole("complementary", { name: "Ada identity" }),
  ).getByLabelText("Name");
  await user.clear(field);
  await user.type(field, "Discarded{Escape}");
  expect(agents2.save).not.toHaveBeenCalled();
  expect(field).toHaveValue("Ada");
  await user.clear(field);
  await user.type(field, "Kept{Enter}");
  expect(agents2.save).toHaveBeenCalledExactlyOnceWith("1".repeat(64), {
    name: "Kept",
  });
});

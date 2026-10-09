// @vitest-environment jsdom
import "@testing-library/jest-dom/vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import { AgentDelete } from "./AgentManage";
import type { Agent, Agents2 } from "./service";

afterEach(cleanup);

const agent = {
  pubkey: "1".repeat(64),
  name: "Ada",
  type: "test/echo",
  owner: "f".repeat(64),
  relay: "wss://relay.example",
  config: {},
  attention: {},
  skipped: {},
  timers: {},
  attentionEnabled: true,
} satisfies Agent;

it("deletes after confirming, and keeps the dialog open with the error when Delete fails", async () => {
  const user = userEvent.setup();
  const remove = vi
    .fn<Agents2["remove"]>()
    .mockRejectedValueOnce(new Error("Credential store refused"))
    .mockResolvedValueOnce();
  const onRemoved = vi.fn();
  render(
    <AgentDelete
      agents2={{ remove } as unknown as Agents2}
      agent={agent}
      onRemoved={onRemoved}
    />,
  );
  await user.click(screen.getByRole("button", { name: "Delete agent" }));
  await user.click(screen.getByRole("button", { name: "Delete" }));
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Credential store refused",
  );
  expect(onRemoved).not.toHaveBeenCalled();
  await user.click(screen.getByRole("button", { name: "Delete" }));
  await vi.waitFor(() => expect(onRemoved).toHaveBeenCalledOnce());
  expect(remove).toHaveBeenCalledWith(agent.pubkey);
});

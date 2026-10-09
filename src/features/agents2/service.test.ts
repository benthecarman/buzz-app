import { Context } from "@deepseek-ai/cordis";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { RelayEvent } from "../relay/events";
import type { LiveBatch, LiveListener } from "../relay/incoming";
import type { RelayData, RelaySnapshot } from "../relay/service";
import type { RelaySession } from "../relay/session";
import type { AgentIdentity, AgentsNative } from "./native";
import { Agents2Service, type AgentType, type Delivery } from "./service";
import { memoryStorage } from "./test-fakes";

// Cleanup's channel step has its own coverage (relay-removal); here only its
// place in the order matters.
const steps: string[] = [];
const leaveChannels = vi.hoisted(() => vi.fn());
vi.mock("../agents/relay-removal", () => ({
  removeAgentFromChannels: leaveChannels,
}));

const viewer = "a".repeat(64);
const other = "b".repeat(64);
const bot = "c".repeat(64);
const sibling = "d".repeat(64);
const channel = "0b8e2a3c-1d4f-4a5b-8c6d-7e8f9a0b1c2d";
const event = (id: string, patch: Partial<RelayEvent> = {}) =>
  ({
    id: id.padEnd(64, "0"),
    pubkey: other,
    kind: 9,
    created_at: 100,
    content: `hello ${id}`,
    tags: [["h", channel]],
    sig: "",
    ...patch,
  }) as unknown as RelayEvent;

function fakeRelay() {
  const live = new Set<LiveListener>();
  const changes = new Set<() => void>();
  const archived = new Set<string>();
  const archives = {
    archived,
    /** The last read did not finish, as when its session closed. */
    unknown: false,
    consent: vi.fn(
      async () => ({ auth: ["auth"] }) as { auth: string[] } | null,
    ),
    ensure: vi.fn(async () => {}),
    refresh: vi.fn(async () => {}),
    state(pubkey: string) {
      if (this.unknown) return "unknown";
      return archived.has(pubkey) ? "archived" : "not-archived";
    },
    writable: true,
    request: vi.fn(async (_: string, pubkey: string) => {
      steps.push("archive");
      archived.add(pubkey);
    }),
  };
  let status = "connected";
  const statuses = new Set<() => void>();
  const session = {
    archives,
    live: {
      snapshot: () => ({ status }),
      subscribe(listener: () => void) {
        statuses.add(listener);
        return () => statuses.delete(listener);
      },
    },
    subscribeLive(listener: LiveListener) {
      live.add(listener);
      return () => live.delete(listener);
    },
  } as unknown as RelaySession;
  let snapshot: RelaySnapshot = {
    status: "ready",
    generation: 1,
    viewer,
    origin: "https://relay.example.test",
    scope: `https://relay.example.test:${viewer}`,
    session,
  };
  const connected_ = snapshot;
  return {
    relay: {
      snapshot: () => snapshot,
      subscribe: (listener: () => void) => {
        changes.add(listener);
        return () => changes.delete(listener);
      },
    } as unknown as RelayData,
    /** Drops the connection, or restores it with a new session. */
    connect(connected: boolean) {
      snapshot = connected
        ? { ...connected_, session: { ...session } as RelaySession }
        : { status: "disconnected", generation: 2, session };
      for (const listener of changes) listener();
    },
    /** Drops and restores the socket inside the same session, as live.ts does. */
    recover() {
      for (const next of ["retrying", "connected"]) {
        status = next;
        for (const listener of statuses) listener();
      }
    },
    emit: (batch: LiveBatch) => {
      for (const listener of live) listener(batch);
    },
    archives,
  };
}

function fakeNative(identities: AgentIdentity[] = []) {
  let published = 0;
  const keys = [bot, sibling];
  const at = (pubkey: string) =>
    identities.findIndex((identity) => identity.pubkey === pubkey);
  const change = (pubkey: string, patch: Partial<AgentIdentity>) => {
    const index = at(pubkey);
    const identity = identities[index];
    if (identity) identities[index] = { ...identity, ...patch };
  };
  const native = {
    list: vi.fn(async () => [...identities]),
    create: vi.fn(async (_: string, __: string, type: string, name: string) => {
      const identity = {
        pubkey: keys.shift() as string,
        relay: "wss://relay.example.test",
        owner: viewer,
        type,
        name,
        deleted: false,
      };
      identities.push(identity);
      return identity;
    }),
    rename: vi.fn(async (pubkey: string, name: string) => {
      change(pubkey, { name });
    }),
    remove: vi.fn(async (pubkey: string) => {
      steps.push("key");
      change(pubkey, { deleted: true });
    }),
    forget: vi.fn(async (pubkey: string) => {
      steps.push("forget");
      identities.splice(at(pubkey), 1);
    }),
    publish: vi.fn(async (pubkey: string, template: { kind: number }) =>
      event(`p${++published}`, { pubkey, kind: template.kind }),
    ),
    publishProfile: vi.fn(async (_: string) => {}),
    query: vi.fn(async () => []),
    upload: vi.fn(async () => ({ url: "", sha256: "", size: 0, type: "" })),
    remember: vi.fn(async (pubkey: string) =>
      event(`m${++published}`, { pubkey }),
    ),
    claim: vi.fn(async (_: string) => true),
    release: vi.fn(async (_: string) => {}),
  } satisfies AgentsNative;
  return native;
}

type Config = { reply: string };
async function setup({
  storage = memoryStorage(),
  identities = [] as AgentIdentity[],
} = {}) {
  const ctx = new Context();
  ctx.provide("pluginStatus", {
    isActive: () => true,
    subscribe: () => () => {},
  });
  const fake = fakeRelay();
  const native = fakeNative(identities);
  const service = new Agents2Service(ctx, fake.relay, native, storage);
  const plugin = ctx.extend({
    pluginOwner: Object.freeze({ id: "example", revision: "one" }),
  });
  const run = vi.fn<(delivery: Delivery<Config>) => void | Promise<void>>();
  const type: AgentType<Config> = {
    id: "echo",
    title: "Echo",
    defaults: () => ({
      config: { reply: "ok" },
      attention: {
        "interest/default": { type: "interest", instructions: "Be brief." },
        "watch/channel": {
          type: "event",
          interest_id: "default",
          enabled: true,
          since: 1,
          channels: [channel],
          kinds: [9],
          filter: 'content == "deploy"',
        },
      },
    }),
    run,
  };
  plugin.agents2.register(type);
  await vi.waitFor(() => expect(service.snapshot().status).toBe("ready"));
  return {
    service,
    native,
    storage,
    run,
    emit: fake.emit,
    connect: fake.connect,
    recover: fake.recover,
    archives: fake.archives,
    ctx,
  };
}

/** Lets every pending promise chain finish: a macrotask runs only after the
 * microtask queue is empty, so this does not depend on how fast runners are. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

beforeEach(() => {
  vi.useRealTimers();
  steps.length = 0;
  leaveChannels.mockReset();
  leaveChannels.mockImplementation(async () => void steps.push("channels"));
});
afterEach(() => vi.useRealTimers());

it("creates an agent in the community shown, with its type's defaults, and publishes its profile", async () => {
  const { service, native } = await setup();
  const agent = await service.create({ type: "example/echo", name: " Echo " });
  expect(native.create).toHaveBeenCalledWith(
    "https://relay.example.test",
    viewer,
    "example/echo",
    "Echo",
  );
  expect(native.publishProfile).toHaveBeenCalledWith(bot);
  expect(agent).toMatchObject({
    pubkey: bot,
    name: "Echo",
    type: "example/echo",
    config: { reply: "ok" },
  });
  expect(Object.keys(agent.attention)).toEqual([
    "interest/default",
    "watch/channel",
  ]);
  expect(service.find(bot)).toEqual(agent);
});

it("delivers mentions and matching watches once, and never the agent's own events", async () => {
  const { service, run, emit } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  const mention = event("m", {
    tags: [
      ["h", channel],
      ["p", bot],
    ],
  });
  const watched = event("w", { content: "deploy" });
  emit({ events: [mention, mention, event("x"), watched], channelId: channel });
  emit({ events: [event("own", { pubkey: bot, content: "deploy" })] });
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(2));
  const [first, second] = run.mock.calls.map(([delivery]) => delivery);
  expect(first?.trigger).toEqual({ type: "mention", event: mention });
  expect(second?.trigger).toMatchObject({
    type: "watch",
    event: watched,
    slug: "watch/channel",
    interest: { instructions: "Be brief." },
  });
  expect(first?.channelId).toBe(channel);
  expect(first?.config).toEqual({ reply: "ok" });
});

it("treats a reply to something the agent published as addressed", async () => {
  const { service, run, emit } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  run.mockImplementationOnce(async ({ agent }) => {
    await agent.publish({ kind: 9, content: "hi", tags: [["h", channel]] });
  });
  emit({ events: [event("m", { tags: [["p", bot]] })] });
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(1));
  // The publish above returned event p1.
  emit({ events: [event("r", { tags: [["e", "p1".padEnd(64, "0")]] })] });
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(2));
  expect(run.mock.calls[1]?.[0].trigger.type).toBe("mention");
});

it("keeps the in-flight run and the queue across edits; each job reads the agent as it starts", async () => {
  const { service, run, emit, native } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  let release: () => void = () => {};
  let held: Delivery<Config> | undefined;
  run.mockImplementationOnce(
    (delivery) =>
      new Promise<void>((resolve) => {
        held = delivery;
        release = resolve;
      }),
  );
  emit({
    events: [
      event("1", { tags: [["p", bot]] }),
      event("2", { tags: [["p", bot]] }),
    ],
  });
  await vi.waitFor(() => expect(held).toBeDefined());
  expect(run).toHaveBeenCalledTimes(1);
  await service.save(bot, { name: "Echo two", config: { reply: "new" } });
  await service.save(bot, {
    attention: { "watch/channel": null },
  });
  expect(held?.signal.aborted).toBe(false);
  await held?.agent.publish({ kind: 7, content: "+" });
  expect(native.publish).toHaveBeenLastCalledWith(bot, {
    kind: 7,
    content: "+",
  });
  release();
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(2));
  const queued = run.mock.calls[1]?.[0];
  expect(queued?.config).toEqual({ reply: "new" });
  expect(queued?.agent.name).toBe("Echo two");
});

it("keeps queued work across a disconnect and runs it once on reconnect", async () => {
  const { service, run, emit, connect } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  let release: () => void = () => {};
  run.mockImplementationOnce(
    () => new Promise<void>((resolve) => (release = resolve)),
  );
  emit({
    events: [
      event("1", { tags: [["p", bot]] }),
      event("2", { tags: [["p", bot]] }),
    ],
  });
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(1));
  connect(false);
  expect(service.find(bot)).toBeUndefined();
  release();
  await settle();
  expect(run).toHaveBeenCalledTimes(1);
  await service.save(bot, { config: { reply: "new" } });
  connect(true);
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(2));
  expect(run.mock.calls[1]?.[0].config).toEqual({ reply: "new" });
  await settle();
  expect(run).toHaveBeenCalledTimes(2);
});

it("aborts the in-flight run, and drops what is queued, when the agent is removed", async () => {
  const { service, run, emit } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  let held: Delivery<Config> | undefined;
  run.mockImplementationOnce(
    (delivery) =>
      new Promise(() => {
        held = delivery;
      }),
  );
  emit({
    events: [
      event("1", { tags: [["p", bot]] }),
      event("2", { tags: [["p", bot]] }),
    ],
  });
  await vi.waitFor(() => expect(held).toBeDefined());
  await service.remove(bot);
  expect(held?.signal.aborted).toBe(true);
  await settle();
  expect(run).toHaveBeenCalledTimes(1);
});

it("drops its events while another copy of the app holds the agent, and runs once it holds it", async () => {
  const { service, run, emit, native } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  native.claim.mockResolvedValue(false);
  emit({ events: [event("1", { tags: [["p", bot]] })] });
  await vi.waitFor(() => expect(native.claim).toHaveBeenCalledWith(bot));
  await settle();
  expect(run).not.toHaveBeenCalled();
  native.claim.mockResolvedValue(true);
  const second = event("2", { tags: [["p", bot]] });
  emit({ events: [second] });
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(1));
  expect(run.mock.calls[0]?.[0].trigger).toEqual({
    type: "mention",
    event: second,
  });
  // When the claim itself fails, it answers rather than stay silent.
  native.claim.mockRejectedValue(new Error("unreadable"));
  emit({ events: [event("3", { tags: [["p", bot]] })] });
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(2));
});

it("lets another copy of the app run an agent it cannot run", async () => {
  vi.useFakeTimers();
  const { service, run, emit, native, connect } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  emit({ events: [event("1", { tags: [["p", bot]] })] });
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(1));
  expect(native.release).not.toHaveBeenCalled();
  connect(false);
  await vi.advanceTimersByTimeAsync(10_000);
  expect(native.release).toHaveBeenCalledTimes(1);
  expect(native.release).toHaveBeenCalledWith(bot);
  connect(true);
  native.claim.mockClear();
  emit({ events: [event("2", { tags: [["p", bot]] })] });
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(2));
  expect(native.claim).toHaveBeenCalledWith(bot);
});

it("is not woken by reactions, deletions or DMs", async () => {
  const { service, run, emit } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  const ownMessage = "p1".padEnd(64, "0");
  emit({
    events: [
      event("react", {
        kind: 7,
        tags: [
          ["e", ownMessage],
          ["p", bot],
        ],
      }),
      event("delete", { kind: 5, tags: [["e", ownMessage]] }),
      event("dm", { kind: 4, tags: [["p", bot]] }),
    ],
  });
  emit({ events: [event("m", { tags: [["p", bot]] })] });
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(1));
  expect(run.mock.calls[0]?.[0].trigger).toMatchObject({ type: "mention" });
});

it("does not let one owner's agents wake each other by replying", async () => {
  const { service, run, emit } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  await service.create({ type: "example/echo", name: "Sibling" });
  run.mockImplementation(async ({ agent, trigger }) => {
    if (trigger.type === "timer") return;
    await agent.publish({ kind: 9, content: "hi" });
  });
  const start = event("start", {
    tags: [
      ["h", channel],
      ["p", bot],
    ],
  });
  emit({ events: [start] });
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(1));
  // The sibling replies to Echo's message, naming it, and also says "deploy",
  // which Echo's watch matches. Neither wakes Echo.
  const echoMessage = "p1".padEnd(64, "0");
  emit({
    events: [
      event("reply", {
        pubkey: sibling,
        content: "deploy",
        tags: [
          ["h", channel],
          ["e", echoMessage],
          ["p", bot],
        ],
      }),
    ],
  });
  await settle();
  expect(run).toHaveBeenCalledTimes(1);
  // A fresh mention from the sibling still does.
  emit({
    events: [
      event("ask", {
        pubkey: sibling,
        tags: [
          ["h", channel],
          ["p", bot],
        ],
      }),
    ],
  });
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(2));
});

it("renames through native, and publishes profiles again on rename and reconnect", async () => {
  const { service, native, connect } = await setup();
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  native.publishProfile.mockRejectedValueOnce(new Error("offline"));
  const agent = await service.create({ type: "example/echo", name: "Echo" });
  // An unpublished profile only warns; the agent exists either way.
  await vi.waitFor(() => expect(warn).toHaveBeenCalled());
  expect(service.find(bot)).toEqual(agent);
  warn.mockRestore();
  native.publishProfile.mockClear();
  await service.save(bot, { name: " Echo Two " });
  expect(native.rename).toHaveBeenCalledWith(bot, "Echo Two");
  expect(service.find(bot)?.name).toBe("Echo Two");
  expect(native.publishProfile).toHaveBeenCalledWith(bot);
  // An unchanged name is not saved again.
  await service.save(bot, { name: "Echo Two", config: { reply: "new" } });
  expect(native.rename).toHaveBeenCalledOnce();
  connect(false);
  connect(true);
  expect(native.publishProfile).toHaveBeenCalledTimes(2);
});

it("retries a failed profile and cleanup when the live connection recovers in the same session", async () => {
  const { service, native, recover } = await setup();
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  native.publishProfile.mockRejectedValueOnce(new Error("offline"));
  await service.create({ type: "example/echo", name: "Echo" });
  await vi.waitFor(() => expect(warn).toHaveBeenCalledTimes(1));
  expect(native.publishProfile).toHaveBeenCalledTimes(1);
  recover();
  expect(native.publishProfile).toHaveBeenCalledTimes(2);
  leaveChannels.mockRejectedValueOnce(new Error("offline"));
  await service.remove(bot);
  await vi.waitFor(() => expect(warn).toHaveBeenCalledTimes(2));
  warn.mockRestore();
  expect(native.forget).not.toHaveBeenCalled();
  recover();
  await vi.waitFor(() => expect(native.forget).toHaveBeenCalledWith(bot));
});

it("retries a cleanup that fails after the connection recovered while it ran", async () => {
  const { service, native, recover } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  let fail: (error: Error) => void = () => {};
  leaveChannels.mockImplementationOnce(
    () => new Promise((_, reject) => (fail = reject)),
  );
  await service.remove(bot);
  await vi.waitFor(() => expect(leaveChannels).toHaveBeenCalledTimes(1));
  recover();
  fail(new Error("Read started before the drop"));
  await vi.waitFor(() => expect(native.forget).toHaveBeenCalledWith(bot));
  expect(leaveChannels).toHaveBeenCalledTimes(2);
  warn.mockRestore();
});

it("shows an agent whose settings could not be saved with its type's defaults", async () => {
  const storage = memoryStorage();
  const { service } = await setup({ storage });
  vi.spyOn(storage, "setItem").mockImplementationOnce(() => {
    throw new Error("QuotaExceededError");
  });
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  await service.create({ type: "example/echo", name: "Echo" });
  warn.mockRestore();
  expect(storage.getItem("buzz.agents2.v1") ?? "").not.toContain(bot);
  expect(service.find(bot)).toMatchObject({ config: { reply: "ok" } });
  expect(Object.keys(service.find(bot)?.attention ?? {})).toEqual([
    "interest/default",
    "watch/channel",
  ]);
  // The next save stores them.
  await service.save(bot, { config: { reply: "saved" } });
  expect(storage.getItem("buzz.agents2.v1")).toContain(bot);
});

it("saves nothing when native create fails", async () => {
  const storage = memoryStorage();
  const { service, native } = await setup({ storage });
  native.create.mockRejectedValueOnce(new Error("Credential store refused"));
  await expect(
    service.create({ type: "example/echo", name: "Echo" }),
  ).rejects.toThrow("Credential store refused");
  expect(service.snapshot().agents).toEqual([]);
  expect(storage.getItem("buzz.agents2.v1")).toBeNull();
});

it("returns the created agent after the community changes mid-create", async () => {
  const { service, native, connect } = await setup();
  const create = native.create.getMockImplementation();
  if (!create) throw new Error("The fake creates agents");
  native.create.mockImplementationOnce(async (...input) => {
    connect(false);
    return create(...input);
  });
  const agent = await service.create({ type: "example/echo", name: "Echo" });
  expect(agent).toMatchObject({ pubkey: bot, name: "Echo" });
  expect(service.find(bot)).toBeUndefined();
});

it("forgets records whose key is gone when identities load", async () => {
  const storage = memoryStorage();
  storage.setItem(
    "buzz.agents2.v1",
    JSON.stringify({
      version: 1,
      agents: {
        [bot]: { pubkey: bot, attention: {}, config: {} },
        [sibling]: { pubkey: sibling, attention: {}, config: {} },
      },
    }),
  );
  const { service } = await setup({
    storage,
    identities: [
      {
        pubkey: bot,
        relay: "wss://relay.example.test",
        owner: viewer,
        type: "example/echo",
        name: "Echo",
        deleted: false,
      },
    ],
  });
  expect(service.snapshot().agents.map((agent) => agent.name)).toEqual([
    "Echo",
  ]);
  expect(storage.getItem("buzz.agents2.v1")).not.toContain(sibling);
});

it("keeps an agent created while identities are still loading", async () => {
  const storage = memoryStorage();
  const ctx = new Context();
  ctx.provide("pluginStatus", {
    isActive: () => true,
    subscribe: () => () => {},
  });
  const native = fakeNative();
  let release: (value: AgentIdentity[]) => void = () => {};
  native.list.mockImplementationOnce(
    () => new Promise<AgentIdentity[]>((resolve) => (release = resolve)),
  );
  const service = new Agents2Service(ctx, fakeRelay().relay, native, storage);
  ctx
    .extend({ pluginOwner: Object.freeze({ id: "example", revision: "one" }) })
    .agents2.register<Config>({
      id: "echo",
      title: "Echo",
      defaults: () => ({ config: { reply: "ok" } }),
    });
  await vi.waitFor(() => expect(native.list).toHaveBeenCalled());
  await service.create({ type: "example/echo", name: "Echo" });
  release([]);
  await vi.waitFor(() => expect(service.snapshot().status).toBe("ready"));
  expect(service.find(bot)?.name).toBe("Echo");
  expect(storage.getItem("buzz.agents2.v1")).toContain(bot);
});

it("fires a due timer once, then again an interval after it ran", async () => {
  vi.useFakeTimers({ now: 1_000_000 });
  const { service, run, storage } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  await service.save(bot, {
    attention: {
      "watch/tick": {
        type: "timer",
        interest_id: "default",
        prompt: "check in",
        enabled: true,
        interval_secs: 60,
        armed_at: 1_000,
        max_occurrences: 2,
        expires_at: null,
      },
      // Armed long ago with no run state: what was due counts as used, so it
      // neither fires at once nor ever (both of its runs are spent).
      "watch/stale": {
        type: "timer",
        interest_id: "default",
        prompt: "stale",
        enabled: true,
        interval_secs: 60,
        armed_at: 0,
        max_occurrences: 2,
        expires_at: null,
      },
    },
  });
  await vi.advanceTimersByTimeAsync(55_000);
  expect(run).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(5_000);
  expect(run).toHaveBeenCalledTimes(1);
  expect(run.mock.calls[0]?.[0].trigger).toMatchObject({
    type: "timer",
    slug: "watch/tick",
    interest: { instructions: "Be brief." },
  });
  await vi.advanceTimersByTimeAsync(30_000);
  expect(run).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(60_000);
  expect(run).toHaveBeenCalledTimes(2);
  await vi.advanceTimersByTimeAsync(120_000);
  expect(run).toHaveBeenCalledTimes(2);
  // Its run state lives in the agent's record and goes when the timer does.
  const stored = () =>
    JSON.parse(storage.getItem("buzz.agents2.v1") ?? "{}").agents[bot];
  expect(stored().timers["watch/tick"]).toMatchObject({ used: 2 });
  expect(stored().timers["watch/stale"]).toMatchObject({ used: 16 });
  await service.save(bot, { attention: { "watch/tick": null } });
  expect(Object.keys(stored().timers)).toEqual(["watch/stale"]);
});

it("keeps at most one occurrence of a timer waiting, and never runs one removed meanwhile", async () => {
  vi.useFakeTimers({ now: 1_000_000 });
  const { service, run } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  const timer = (prompt: string) =>
    ({
      type: "timer",
      interest_id: "default",
      prompt,
      enabled: true,
      interval_secs: 1,
      armed_at: 1_000,
      max_occurrences: null,
      expires_at: null,
    }) as const;
  await service.save(bot, {
    attention: { "watch/kept": timer("kept"), "watch/removed": timer("gone") },
  });
  let release: () => void = () => {};
  run.mockImplementationOnce(
    () => new Promise<void>((resolve) => (release = resolve)),
  );
  // The first run holds the runner across many intervals of both timers.
  await vi.advanceTimersByTimeAsync(5_000);
  expect(run).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(30_000);
  await service.save(bot, { attention: { "watch/removed": null } });
  release();
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(2));
  await vi.advanceTimersByTimeAsync(0);
  const slugs = run.mock.calls.map(([{ trigger }]) =>
    trigger.type === "timer" ? trigger.slug : trigger.type,
  );
  expect(slugs).toEqual(["watch/kept", "watch/kept"]);
});

it("gives each due timer a turn when one's runs outlast its interval", async () => {
  vi.useFakeTimers({ now: 1_000_000 });
  const { service, run } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  const timer = (prompt: string) =>
    ({
      type: "timer",
      interest_id: "default",
      prompt,
      enabled: true,
      interval_secs: 1,
      armed_at: 1_000,
      max_occurrences: null,
      expires_at: null,
    }) as const;
  await service.save(bot, {
    attention: { "watch/slow": timer("slow"), "watch/other": timer("other") },
  });
  // Every run takes three intervals, so the first timer is always due again.
  run.mockImplementation(
    () => new Promise<void>((resolve) => setTimeout(resolve, 3_000)),
  );
  await vi.advanceTimersByTimeAsync(20_000);
  const slugs = run.mock.calls.map(([{ trigger }]) =>
    trigger.type === "timer" ? trigger.slug : trigger.type,
  );
  expect(slugs.slice(0, 4)).toEqual([
    "watch/slow",
    "watch/other",
    "watch/slow",
    "watch/other",
  ]);
});

it("runs waiting events before a due timer, so a busy timer cannot starve them", async () => {
  vi.useFakeTimers({ now: 1_000_000 });
  const { service, run, emit } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  await service.save(bot, {
    attention: {
      "watch/tick": {
        type: "timer",
        interest_id: "default",
        prompt: "tick",
        enabled: true,
        interval_secs: 1,
        armed_at: 1_000,
        max_occurrences: null,
        expires_at: null,
      },
    },
  });
  let release: () => void = () => {};
  run.mockImplementationOnce(
    () => new Promise<void>((resolve) => (release = resolve)),
  );
  await vi.advanceTimersByTimeAsync(5_000);
  emit({ events: [event("m", { tags: [["p", bot]] })] });
  await vi.advanceTimersByTimeAsync(5_000);
  release();
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(3));
  expect(run.mock.calls.map(([{ trigger }]) => trigger.type)).toEqual([
    "timer",
    "mention",
    "timer",
  ]);
});

it("deletes the key at once, then leaves its channels and archives it before forgetting it", async () => {
  const { service, native, storage, archives } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  await service.remove(bot);
  expect(service.snapshot().agents).toEqual([]);
  expect(storage.getItem("buzz.agents2.v1")).not.toContain(bot);
  await vi.waitFor(() =>
    expect(steps).toEqual(["key", "channels", "archive", "forget"]),
  );
  expect(leaveChannels).toHaveBeenCalledWith(
    expect.objectContaining({ archives }),
    bot,
    expect.any(AbortSignal),
  );
  expect(archives.request).toHaveBeenCalledWith(
    "archive",
    bot,
    expect.any(AbortSignal),
  );
  expect(await native.list()).toEqual([]);
});

it("retries a failed cleanup on the next connect, without archiving twice", async () => {
  const { service, native, archives, connect } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  leaveChannels.mockRejectedValueOnce(new Error("Channel removal refused"));
  await service.remove(bot);
  await vi.waitFor(() => expect(warn).toHaveBeenCalledTimes(1));
  archives.request.mockRejectedValueOnce(new Error("Archive refused"));
  connect(true);
  await vi.waitFor(() => expect(warn).toHaveBeenCalledTimes(2));
  warn.mockRestore();
  expect(native.forget).not.toHaveBeenCalled();
  archives.request.mockClear();
  archives.archived.add(bot);
  connect(true);
  await vi.waitFor(() => expect(native.forget).toHaveBeenCalledWith(bot));
  expect(archives.request).not.toHaveBeenCalled();
});

it("forgets an agent with no owner-attested profile without archiving it", async () => {
  const { service, native, archives } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  archives.consent.mockResolvedValueOnce(null);
  await service.remove(bot);
  await vi.waitFor(() => expect(native.forget).toHaveBeenCalledWith(bot));
  expect(archives.request).not.toHaveBeenCalled();
});

it("deletes outside the agent's community, and cleans up once it is open", async () => {
  const { service, native, connect } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  connect(false);
  await service.remove(bot);
  expect(native.remove).toHaveBeenCalledWith(bot);
  await settle();
  expect(leaveChannels).not.toHaveBeenCalled();
  connect(true);
  await vi.waitFor(() => expect(native.forget).toHaveBeenCalledWith(bot));
});

it("keeps the cleanup for later when the session closes while archive state is read", async () => {
  const { service, native, archives, connect } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  archives.refresh.mockImplementationOnce(async () => {
    // A closed session's read ends without an answer.
    archives.unknown = true;
    connect(false);
  });
  await service.remove(bot);
  await vi.waitFor(() => expect(archives.refresh).toHaveBeenCalled());
  await settle();
  expect(native.forget).not.toHaveBeenCalled();
  expect(await native.list()).toMatchObject([{ pubkey: bot, deleted: true }]);
  archives.unknown = false;
  connect(true);
  await vi.waitFor(() => expect(native.forget).toHaveBeenCalledWith(bot));
  expect(archives.request).toHaveBeenCalledOnce();
});

it("runs once per matching watch, and passes a classifier watch it cannot classify", async () => {
  const { service, run, emit } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  await service.save(bot, {
    attention: {
      "watch/classified": {
        type: "event",
        interest_id: "missing",
        enabled: true,
        since: 1,
        channels: "all",
        kinds: [],
        classifier: {
          questions: {
            deploy: {
              question: "Is it about a deploy?",
              true: "It is",
              false: "It is not",
              threshold: 0.5,
            },
          },
        },
      },
    },
  });
  emit({ events: [event("w", { content: "deploy" })] });
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(2));
  const triggers = run.mock.calls.map(([delivery]) => delivery.trigger);
  expect(triggers).toEqual([
    expect.objectContaining({ slug: "watch/channel" }),
    expect.objectContaining({
      slug: "watch/classified",
      classifier: "not run",
    }),
  ]);
  // Its Interest does not exist, so it arrives without instructions.
  expect(triggers[1]).not.toHaveProperty("interest");
});

it("keeps DMs and reactions to its own messages away from watches", async () => {
  const { service, run, emit } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  await service.save(bot, {
    attention: {
      "watch/everything": {
        type: "event",
        interest_id: "default",
        enabled: true,
        since: 1,
        channels: "all",
        kinds: [],
      },
    },
  });
  run.mockImplementationOnce(async ({ agent }) => {
    await agent.publish({ kind: 9, content: "hi", tags: [["h", channel]] });
  });
  emit({ events: [event("m", { tags: [["p", bot]] })] });
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(1));
  emit({
    events: [
      event("dm", { kind: 4, tags: [["p", bot]] }),
      // p1 is what the run above published.
      event("like", { kind: 7, tags: [["e", "p1".padEnd(64, "0")]] }),
      event("note", { kind: 1 }),
    ],
  });
  await vi.waitFor(() => expect(run).toHaveBeenCalledTimes(2));
  await settle();
  expect(run).toHaveBeenCalledTimes(2);
  expect(run.mock.calls[1]?.[0].trigger).toMatchObject({
    type: "watch",
    slug: "watch/everything",
    event: { id: "note".padEnd(64, "0") },
  });
});

it("keeps an Interest while a watch still uses it", async () => {
  const { service } = await setup();
  await service.create({ type: "example/echo", name: "Echo" });
  await expect(
    service.save(bot, { attention: { "interest/default": null } }),
  ).rejects.toThrow(/use this Interest/);
  expect(service.find(bot)?.attention).toHaveProperty("interest/default");
  await service.save(bot, {
    attention: { "watch/channel": null, "interest/default": null },
  });
  expect(service.find(bot)?.attention).toEqual({});
});

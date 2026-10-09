import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { RelayEvent } from "../../features/relay/events";
import type { LiveCallbacks } from "../../features/relay/live";
import { createReminders } from "../../features/relay/reminders";
import { Context } from "@deepseek-ai/cordis";
import { PluginRuntime } from "../../plugins/runtime";
import type { PluginModule } from "../../plugins/api";
import { provideNavigation } from "../../features/navigation/service";
import { NotificationsService } from "../../features/notifications/service";
import { createNotificationPreferences } from "../../features/notifications/preferences";
import type { NotificationPlatform } from "../../features/notifications/platform";
import { apply } from "./index";

const viewer = "a".repeat(64);
const target = {
  eventId: "e".repeat(64),
  channelId: "c",
  preview: "hi",
  authorPubkey: "a",
};
const wire = (id: string, notBefore: number): RelayEvent =>
  ({
    id,
    pubkey: viewer,
    kind: 30300,
    created_at: 1,
    tags: [
      ["d", id],
      ["not_before", String(notBefore)],
    ],
    content: JSON.stringify({ target, status: "pending" }),
    sig: "",
  }) as unknown as RelayEvent;

function session(
  history: Promise<RelayEvent[]> | (() => Promise<RelayEvent[]>),
) {
  const reminders = createReminders({
    viewer,
    signal: new AbortController().signal,
    host: {
      decode: async (events) =>
        events.map((e) => ({ eventId: e.id, content: JSON.parse(e.content) })),
      sign: async ({ d, createdAt, notBefore, content }) =>
        ({
          id: d,
          pubkey: viewer,
          kind: 30300,
          created_at: createdAt,
          tags: [
            ["d", d],
            ["not_before", String(notBefore)],
          ],
          content: JSON.stringify(content),
          sig: "",
        }) as unknown as RelayEvent,
    },
    query: typeof history === "function" ? history : () => history,
    publish: async () => {},
  });
  // These sessions are already live, as after establishment.
  void reminders.recover();
  return reminders;
}

let liveStatus = "connected";
const live = {
  subscribe: () => () => {},
  snapshot: () => ({ status: liveStatus, roster: { state: "verified" } }),
};
const channels = {
  list: () => ({
    status: "ready",
    channels: [{ id: "c", name: "general", channelType: "stream" }],
  }),
};

function mount(notifications?: unknown) {
  const listeners = new Set<() => void>();
  let snapshot: unknown = { status: "connecting" };
  const submit = vi.fn(() => Promise.resolve());
  const stop: (() => void)[] = [];
  const ctx = {
    relay: {
      snapshot: () => snapshot,
      subscribe(listener: () => void) {
        listeners.add(listener);
        return () => void listeners.delete(listener);
      },
    },
    notifications: notifications ?? { register: () => ({ submit }) },
    pages: { register: vi.fn() },
    navigation: {},
    conversation: { registerMessageAction: vi.fn() },
    effect: (body: () => () => void) => void stop.push(body()),
  };
  apply(ctx as unknown as Parameters<typeof apply>[0]);
  const connect = (scope: string, reminders: unknown) => {
    snapshot = {
      status: "ready",
      scope,
      viewer,
      session: { reminders, channels, live },
    };
    for (const listener of listeners) listener();
  };
  const dispose = () => {
    for (const body of stop.splice(0)) body();
  };
  return { submit, connect, dispose };
}

const at = (seconds: number) => vi.setSystemTime(seconds * 1000);
const flush = () => vi.advanceTimersByTimeAsync(0);

let stored: Map<string, string>;
beforeEach(() => {
  stored = new Map();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => stored.get(key) ?? null,
    setItem: (key: string, value: string) => void stored.set(key, value),
  });
  vi.useFakeTimers();
  at(100);
  liveStatus = "connected";
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

it("notifies a reminder from slow history even after a live update moved the clock", async () => {
  let resolveHistory: (events: RelayEvent[]) => void = () => {};
  const model = session(new Promise((resolve) => (resolveHistory = resolve)));
  const { submit, connect } = mount();
  connect(`origin:${viewer}`, model.capability);
  at(105);
  model.receive([wire("later", 10_000)]);
  await flush();
  at(106);
  resolveHistory([wire("overdue", 103)]);
  await flush();
  expect(submit).toHaveBeenCalledExactlyOnceWith(
    expect.objectContaining({ sourceKey: "overdue:103" }),
  );
  model.receive([wire("other", 20_000)]);
  await flush();
  expect(submit).toHaveBeenCalledTimes(1);
});

it("suppresses reminders already due when a community is first bound, and fires late timers", async () => {
  const { submit, connect } = mount();
  const first = session(Promise.resolve([wire("old", 50), wire("soon", 110)]));
  connect(`a:${viewer}`, first.capability);
  await flush();
  expect(submit).not.toHaveBeenCalled();
  at(120);
  await vi.advanceTimersByTimeAsync(10_000);
  expect(submit).toHaveBeenCalledExactlyOnceWith(
    expect.objectContaining({ sourceKey: "soon:110" }),
  );
  at(200);
  const second = session(Promise.resolve([wire("b-old", 150)]));
  connect(`b:${viewer}`, second.capability);
  await flush();
  expect(submit).toHaveBeenCalledTimes(1);
});

function deferred() {
  let resolve: (events: RelayEvent[]) => void = () => {};
  let reject: (error: Error) => void = () => {};
  const promise = new Promise<RelayEvent[]>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

it("notifies a reminder from slow history after a local save during loading", async () => {
  const history = deferred();
  const model = session(history.promise);
  const { submit, connect } = mount();
  connect(`origin:${viewer}`, model.capability);
  at(105);
  await model.capability.create(target, 10_000);
  await flush();
  at(106);
  history.resolve([wire("overdue", 103)]);
  await flush();
  expect(submit).toHaveBeenCalledExactlyOnceWith(
    expect.objectContaining({ sourceKey: "overdue:103" }),
  );
});

it("keeps the bind-time window through a failed first read until a refresh succeeds", async () => {
  const first = deferred();
  const reads = [first.promise, Promise.resolve([wire("overdue", 103)])];
  const model = session(() => reads.shift() as Promise<RelayEvent[]>);
  const { submit, connect } = mount();
  connect(`origin:${viewer}`, model.capability);
  model.receive([wire("later", 10_000)]);
  await flush();
  at(105);
  first.reject(new Error("offline"));
  await flush();
  at(106);
  await model.capability.refresh();
  await flush();
  expect(submit).toHaveBeenCalledExactlyOnceWith(
    expect.objectContaining({ sourceKey: "overdue:103" }),
  );
});

it("keeps the bind-time window when a live arrival follows a failed first read", async () => {
  const reads = [
    Promise.reject(new Error("offline")),
    Promise.resolve([wire("overdue", 103)]),
  ];
  const model = session(() => reads.shift() as Promise<RelayEvent[]>);
  const { submit, connect } = mount();
  connect(`origin:${viewer}`, model.capability);
  await flush();
  expect(model.capability.snapshot().status).toBe("error");
  at(105);
  model.receive([wire("later", 10_000)]);
  await flush();
  at(106);
  await model.capability.refresh();
  await flush();
  expect(submit).toHaveBeenCalledExactlyOnceWith(
    expect.objectContaining({ sourceKey: "overdue:103" }),
  );
});

it("opens a note-only reminder's notification on its own community's page", async () => {
  const origin = "https://a.example";
  const note = {
    ...wire("note", 110),
    content: JSON.stringify({ note: "call back", status: "pending" }),
  } as RelayEvent;
  const { submit, connect } = mount();
  const model = session(Promise.resolve([note]));
  connect(`${origin}:${viewer}`, model.capability);
  await flush();
  at(120);
  await vi.advanceTimersByTimeAsync(10_000);
  connect(
    `https://b.example:${viewer}`,
    session(Promise.resolve([])).capability,
  );
  await flush();
  expect(submit).toHaveBeenCalledExactlyOnceWith({
    sourceKey: "note:110",
    title: "Reminder due",
    body: "call back",
    target: {
      version: 1,
      kind: "page",
      pluginId: "buzz.reminders",
      pageId: "reminders",
      scope: { viewer, communityOrigin: origin },
    },
  });
});

it("names the channel and the reminder in banners through the real notification service", async () => {
  const ctx = new Context();
  let module: PluginModule = { apply() {} };
  const runtime = new PluginRuntime(ctx, async () => module);
  const show = vi.fn<NotificationPlatform["show"]>(async () => {});
  const service = new NotificationsService(
    ctx,
    provideNavigation(ctx).navigation,
    {
      label: "Test",
      permission: async () => "granted",
      requestPermission: async () => "granted",
      show,
      dispose() {},
    },
    createNotificationPreferences(undefined),
    () => true,
    () => {},
  );
  service.selectViewer(viewer);
  let host: unknown;
  module = {
    inject: ["notifications"],
    apply(plugin) {
      host = plugin.notifications;
    },
  };
  runtime.reconcile([
    {
      manifest: { id: "buzz.reminders", name: "Reminders", apiVersion: 1 },
      enabled: true,
      source: "bundled",
      revision: "v1",
      previous: null,
      reloadable: true,
      error: null,
    },
  ]);
  await vi.waitFor(() => expect(host).toBeDefined());
  const elsewhere = { ...target, channelId: "gone", preview: "" };
  const note = (id: string, content: object) => ({
    ...wire(id, 110),
    content: JSON.stringify({ ...content, status: "pending" }),
  });
  const { connect } = mount(host);
  connect(
    `https://a.example:${viewer}`,
    session(
      Promise.resolve([
        wire("known", 110),
        note("unknown", { target: elsewhere, note: "follow up" }),
        note("bare", { target: elsewhere }),
        note("note", { note: "call **back**" }),
        note("definition", {
          target: { ...target, preview: "[ref]: https://example.com/private" },
          note: "see note",
        }),
      ]),
    ).capability,
  );
  await flush();
  at(120);
  await vi.advanceTimersByTimeAsync(10_000);
  expect(show.mock.calls.map(([item]) => item)).toEqual([
    expect.objectContaining({ title: "Reminder due · #general", body: "hi" }),
    expect.objectContaining({ title: "Reminder due", body: "follow up" }),
    expect.objectContaining({
      title: "Reminder due",
      body: "A reminder is waiting",
    }),
    expect.objectContaining({ title: "Reminder due", body: "call back" }),
    expect.objectContaining({
      title: "Reminder due · #general",
      body: "see note",
    }),
  ]);
  runtime.dispose();
  await ctx.fiber.dispose();
});

const checked = (origin: string) =>
  stored.get(`buzz-reminders-checked.v1:${origin}:${viewer}`);

it("seeds a first launch to now and keeps communities apart", async () => {
  stored.set(`buzz-reminders-checked.v1:b:${viewer}`, "not a time");
  const { submit, connect } = mount();
  connect(
    `a:${viewer}`,
    session(Promise.resolve([wire("a-old", 50)])).capability,
  );
  await flush();
  connect(
    `b:${viewer}`,
    session(Promise.resolve([wire("b-old", 60)])).capability,
  );
  await flush();
  expect(submit).not.toHaveBeenCalled();
  expect(checked("a")).toBe("100");
  expect(checked("b")).toBe("100");
  stored.set(`buzz-reminders-checked.v1:a:${viewer}`, "40");
  const again = mount();
  again.connect(
    `a:${viewer}`,
    session(Promise.resolve([wire("a-old", 50)])).capability,
  );
  await flush();
  again.connect(
    `b:${viewer}`,
    session(Promise.resolve([wire("b-old", 60)])).capability,
  );
  await flush();
  expect(again.submit).toHaveBeenCalledExactlyOnceWith(
    expect.objectContaining({ sourceKey: "a-old:50" }),
  );
});

// Roster pages per launch shape: "two" puts the channel on page two of 501,
// "capped" returns full pages until discovery stops at its retained cap.
const ROSTERS = { one: 1, empty: 1, two: 2, capped: 3 } as const;

type Launch = Readonly<{
  /** Which loads first at startup: reminder history or the channel list. */
  first: "history" | "list";
  roster?: keyof typeof ROSTERS;
  permission?: "granted" | "denied";
  member?: boolean;
  enabled?: boolean;
  category?: boolean;
  /** A live roster for a first-page channel arrives before any page. */
  early?: boolean;
  /** That channel is revoked between pages, so discovery restarts. */
  revoke?: boolean;
  /** Reopen as the same account, sharing its saved check time. */
  key?: ReturnType<typeof import("../../features/relay/testing").keypair>;
  banners: 0 | 1;
  /** The live connection never comes up unless the test establishes it. */
  offline?: boolean;
}>;

// Launch with the real session, notification service and access check, a
// reminder that came due while closed, and a channel roster served by page.
async function launch(options: Launch) {
  const { createRelaySession } = await import("../../features/relay/session");
  const { keypair, signed, roster, metadata } = await import(
    "../../features/relay/testing"
  );
  const { notificationAuthorized } = await import(
    "../../features/notifications/messages"
  );
  const key = options.key ?? keypair();
  const relayKey = keypair();
  const origin = "https://restart.example";
  const scope = `${origin}:${key.pubkey}`;
  const pages: ((events: RelayEvent[]) => void)[] = [];

  let live: LiveCallbacks | undefined;
  const reminder = (d: string, notBefore: number, createdAt = 90) =>
    signed(key, {
      kind: 30300,
      created_at: createdAt,
      tags: [
        ["d", d.repeat(32)],
        ["not_before", String(notBefore)],
      ],
      content: JSON.stringify({
        status: "pending",
        target: { ...target, channelId: "room" },
      }),
    });
  const history = [reminder("a", 1000)];
  // While held, history reads wait for `answerHistory`.
  // "list" first: the recovery read on establishment answers only at start.
  let holdHistory = options.first === "list";
  const heldHistory: (() => void)[] = [];
  // Decoding of live events waits for `answerDecode` while held.
  let holdDecode = false;
  const heldDecode: (() => void)[] = [];
  const owner = createRelaySession(
    {
      viewer: key.pubkey,
      subscribe(callbacks) {
        live = callbacks;
        return { update() {}, retry() {}, dispose() {} };
      },
      scope: origin,
      relayAuthor: relayKey.pubkey,
      query: async (filters) =>
        filters.some((f) => f.kinds?.includes(30300))
          ? holdHistory
            ? new Promise((resolve) => {
                const response = [...history];
                heldHistory.push(() => resolve(response));
              })
            : [...history]
          : filters.some((f) => f.kinds?.includes(39002))
            ? new Promise((resolve) => pages.push(resolve))
            : filters.some((f) => f.kinds?.includes(39000))
              ? [metadata(relayKey, "room", "Room")]
              : [],
      media: () => undefined,
      writer: {
        sign: async () => {
          throw new Error("unused");
        },
        publish: async () => {},
      },
      reminders: {
        decode: async (events) => {
          if (holdDecode)
            await new Promise<void>((resolve) => heldDecode.push(resolve));
          return events.map((e) => ({
            eventId: e.id,
            content: JSON.parse(e.content),
          }));
        },
        sign: async () => {
          throw new Error("unused");
        },
      },
    },
    { prepared: true, warm: false },
  );
  // The live subscription is established on launch, before any test step.
  if (options.offline) live?.state({ status: "retrying", routes: [] });
  else {
    live?.state({ status: "connected", routes: [] });
    live?.established();
  }
  await vi.advanceTimersByTimeAsync(0);
  const ctx = new Context();
  const runtime = new PluginRuntime(ctx, async () => ({
    inject: ["notifications", "relay", "navigation", "pages", "conversation"],
    apply,
  }));
  const connectionListeners = new Set<() => void>();
  let snapshot = {
    status: "ready",
    viewer: key.pubkey,
    scope,
    session: owner.session,
    generation: 1,
  };
  const connection = {
    snapshot: () => snapshot,
    subscribe(listener: () => void) {
      connectionListeners.add(listener);
      return () => void connectionListeners.delete(listener);
    },
  };
  const communities = {
    relay: connection,
    snapshot: () => ({
      viewer: key.pubkey,
      selected: origin,
      memberships: [{ id: origin, name: "Restart" }],
    }),
    subscribe: () => () => {},
  };
  const show = vi.fn(async () => {});
  const permission = async () => options.permission ?? "granted";
  const preferences = createNotificationPreferences(undefined);
  new NotificationsService(
    ctx,
    provideNavigation(ctx).navigation,
    {
      label: "Test",
      permission,
      requestPermission: permission,
      show,
      dispose() {},
    },
    preferences,
    (item) => notificationAuthorized(communities as never, item),
    () => {},
  ).selectViewer(key.pubkey);
  preferences.update({
    enabled: options.enabled ?? true,
    categories: { "buzz.reminders/reminders": options.category ?? true },
  });
  ctx.provide("relay", connection as never);
  ctx.provide("pages", { register() {} } as never);
  ctx.provide("conversation", { registerMessageAction() {} } as never);
  owner.session.channels.ensureList();
  const shape = options.roster ?? "one";
  const filler = (page: number, count: number) =>
    Array.from({ length: count }, (_, i) =>
      roster(relayKey, `c${page}-${i}`, [key.pubkey], 1e6 - page * 500 - i),
    );
  const room = (at: number) =>
    roster(relayKey, "room", options.member === false ? [] : [key.pubkey], at);
  const contents = (page: number) =>
    shape === "empty"
      ? []
      : shape === "two"
        ? page === 0
          ? filler(0, 500)
          : [room(1)]
        : shape === "capped"
          ? page === 0
            ? [room(2e6), ...filler(0, 499)]
            : filler(page, 500)
          : [room(1e6)];
  let served = 0;
  return {
    key,
    show,
    pages: ROSTERS[shape],
    saved: () => stored.get(`buzz-reminders-checked.v1:${scope}`),
    emitRoster: () =>
      live?.receive([roster(relayKey, "c0-0", [key.pubkey], 1e6)]),
    /** Revoke a first-page channel mid-scan, then skip the aborted read. */
    async revoke() {
      live?.state({ status: "connected", routes: [] });
      live?.receive([roster(relayKey, "c0-0", [], 2e6)]);
      await vi.waitFor(() =>
        expect(owner.session.live.snapshot().roster.state).toBe("pending"),
      );
      await flush();
      served++;
    },
    roster: () => owner.session.live.snapshot().roster.state,
    reminder,
    history,
    reminders: () => owner.session.reminders?.snapshot(),
    holdHistory(held: boolean) {
      holdHistory = held;
    },
    answerHistory(count = heldHistory.length) {
      for (const answer of heldHistory.splice(0, count)) answer();
    },
    holdDecode(held: boolean) {
      holdDecode = held;
    },
    answerDecode() {
      for (const answer of heldDecode.splice(0)) answer();
    },
    swap(reminders: typeof owner.session.reminders) {
      snapshot = { ...snapshot, session: { ...owner.session, reminders } };
      for (const listener of connectionListeners) listener();
    },
    drop: () => live?.state({ status: "retrying", routes: [] }),
    /** The live connection comes up; its recovery read is queued. */
    establish() {
      live?.state({ status: "connected", routes: [] });
      live?.established();
    },
    /** The subscription reports established without a connected state. */
    announce: () => live?.established(),
    liveStatus: () => owner.session.live.snapshot().status,
    read: () => owner.session.reminders?.refresh(),
    /** The live connection drops, then re-establishes. */
    async reconnect() {
      live?.state({ status: "retrying", routes: [] });
      await flush();
      live?.state({ status: "connected", routes: [] });
      live?.established();
    },
    /** A live delivery from another device. */
    deliver: (event: RelayEvent) => live?.receive([event], { phase: "live" }),
    /** Any live-state callback, which runs a reminders check. */
    poke: () => live?.state({ status: "connected", routes: [] }),
    seed: () => stored.set(`buzz-reminders-checked.v1:${scope}`, "100"),
    /** Answer the next roster read and let its commit settle. */
    async nextPage() {
      const page = served++;
      await vi.waitFor(() => expect(pages[page]).toBeDefined());
      pages[page]?.(contents(page % ROSTERS[shape]));
      await vi.waitFor(() =>
        expect(owner.session.channels.list().status).toBe("ready"),
      );
      await flush();
    },
    async start() {
      if (!options.offline) live?.state({ status: "connected", routes: [] });
      runtime.reconcile([
        {
          manifest: { id: "buzz.reminders", name: "Reminders", apiVersion: 1 },
          enabled: true,
          source: "bundled",
          revision: "v1",
          previous: null,
          reloadable: true,
          error: null,
        },
      ]);
      if (options.first === "list") {
        await flush();
        holdHistory = false;
        for (const answer of heldHistory.splice(0)) answer();
      }
      await vi.waitFor(() =>
        expect(owner.session.reminders?.snapshot().hydrated).toBe(true),
      );
      await flush();
    },
    /** Turn everything back on and reread history; nothing may replay. */
    async refresh() {
      preferences.update({
        enabled: true,
        categories: { "buzz.reminders/reminders": true },
      });
      await owner.session.reminders?.refresh();
      await flush();
    },
    async close() {
      await runtime.dispose();
      await ctx.fiber.dispose();
      owner.dispose();
    },
  };
}

it.each<[string, Launch["first"], Launch["banners"], Partial<Launch>]>([
  ["history before a one-page list", "history", 1, {}],
  ["list before history", "list", 1, {}],
  ["live roster, then history", "list", 1, { roster: "two", early: true }],
  ["history, then live roster", "history", 1, { roster: "two", early: true }],
  ["history before two pages", "history", 1, { roster: "two" }],
  ["two pages before history", "list", 1, { roster: "two" }],
  ["history before a capped scan", "history", 1, { roster: "capped" }],
  ["revocation between pages", "history", 1, { roster: "two", revoke: true }],
  ["notifications off", "list", 0, { enabled: false }],
  ["Reminders category off", "list", 0, { category: false }],
  ["permission denied", "list", 0, { permission: "denied" }],
  ["missing membership", "list", 0, { member: false }],
  ["an empty roster", "list", 0, { roster: "empty" }],
])(
  "catches up a reminder missed while closed: %s",
  async (_, first, banners, rest) => {
    const app = await launch({ first, banners, ...rest });
    const held = () => {
      expect(app.show).not.toHaveBeenCalled();
      expect(app.saved()).toBe("100");
    };
    app.seed();
    at(5_000);
    try {
      if (first === "history") {
        await app.start();
        held();
      }
      if (rest.early) {
        app.emitRoster();
        await flush();
        if (first === "list") await app.start();
        held();
      }
      // Until discovery finishes, nothing notifies and the window is kept.
      for (let page = 1; page < app.pages; page++) {
        await app.nextPage();
        held();
      }
      // An interrupted scan restarts by itself and holds the window until done.
      if (rest.revoke) {
        await app.revoke();
        held();
        for (let page = 1; page < app.pages; page++) {
          await app.nextPage();
          held();
        }
      }
      await app.nextPage();
      if (first === "list" && !rest.early) await app.start();
      await vi.waitFor(() => expect(app.show).toHaveBeenCalledTimes(banners));
      await vi.waitFor(() => expect(app.saved()).toBe("5000"));
      if (rest.revoke) expect(app.roster()).toBe("verified");
      await app.refresh();
      expect(app.show).toHaveBeenCalledTimes(banners);
    } finally {
      await app.close();
    }
  },
);

it("does not replay a caught-up reminder after closing and reopening", async () => {
  const first = await launch({ first: "history", banners: 1 });
  first.seed();
  at(5_000);
  try {
    await first.start();
    await first.nextPage();
    await vi.waitFor(() => expect(first.show).toHaveBeenCalledOnce());
    await vi.waitFor(() => expect(first.saved()).toBe("5000"));
  } finally {
    await first.close();
  }
  at(6_000);
  const reopened = await launch({
    first: "history",
    banners: 0,
    key: first.key,
  });
  try {
    await reopened.start();
    await reopened.nextPage();
    await vi.waitFor(() => expect(reopened.saved()).toBe("6000"));
    expect(reopened.show).not.toHaveBeenCalled();
  } finally {
    await reopened.close();
  }
});

it("notifies a reminder recovered after a reconnect, once, without saving past it first", async () => {
  const app = await launch({ first: "history", banners: 1 });
  app.seed();
  at(5_000);
  try {
    await app.start();
    await app.nextPage();
    await vi.waitFor(() => expect(app.show).toHaveBeenCalledOnce());
    await vi.waitFor(() => expect(app.saved()).toBe("5000"));
    // Another device saved a reminder due at 5500 while this one was offline.
    app.history.push(app.reminder("b", 5_500, 5_400));
    app.holdHistory(true);
    at(6_000);
    await app.reconnect();
    // The list is stale from the drop until the recovery read lands.
    expect(app.reminders()?.status).toBe("loading");
    await app.nextPage();
    app.poke();
    await flush();
    expect(app.saved()).toBe("5000");
    expect(app.show).toHaveBeenCalledOnce();
    app.answerHistory();
    await vi.waitFor(() => expect(app.show).toHaveBeenCalledTimes(2));
    await vi.waitFor(() => expect(app.saved()).toBe("6000"));
    // Further reconnects and reads never repeat either banner.
    app.holdHistory(false);
    await app.reconnect();
    await app.nextPage();
    await app.refresh();
    expect(app.show).toHaveBeenCalledTimes(2);
  } finally {
    await app.close();
  }
});

it("notifies a live reminder decoded after a check passed its due time", async () => {
  const app = await launch({ first: "history", banners: 1 });
  app.seed();
  at(5_000);
  try {
    await app.start();
    await app.nextPage();
    await vi.waitFor(() => expect(app.show).toHaveBeenCalledOnce());
    app.holdDecode(true);
    app.deliver(app.reminder("c", 5_800, 5_100));
    at(6_000);
    app.poke();
    await flush();
    await vi.waitFor(() => expect(app.saved()).toBe("6000"));
    app.answerDecode();
    await vi.waitFor(() => expect(app.show).toHaveBeenCalledTimes(2));
    app.holdDecode(false);
    await app.refresh();
    expect(app.show).toHaveBeenCalledTimes(2);
  } finally {
    await app.close();
  }
});

it("does not repeat a banner when the same community swaps to a new session", async () => {
  const app = mount();
  stored.set(`buzz-reminders-checked.v1:a:${viewer}`, "40");
  at(100);
  app.connect(
    `a:${viewer}`,
    session(Promise.resolve([wire("due", 50)])).capability,
  );
  await flush();
  app.connect(
    `a:${viewer}`,
    session(Promise.resolve([wire("due", 50)])).capability,
  );
  await flush();
  expect(app.submit).toHaveBeenCalledExactlyOnceWith(
    expect.objectContaining({ sourceKey: "due:50" }),
  );
  app.dispose();
});

it("keeps a shown reminder silent after an empty same-community session list", async () => {
  const app = await launch({ first: "history", banners: 1 });
  app.seed();
  at(5_000);
  try {
    await app.start();
    await app.nextPage();
    await vi.waitFor(() => expect(app.show).toHaveBeenCalledOnce());
    const replacement = createReminders({
      viewer: app.key.pubkey,
      signal: new AbortController().signal,
      host: {
        decode: async (events) =>
          events.map((e) => ({
            eventId: e.id,
            content: JSON.parse(e.content),
          })),
        sign: async () => {
          throw new Error("unused");
        },
      },
      query: async () => [],
      publish: async () => {},
    });
    app.swap(replacement.capability);
    await flush();
    expect(replacement.capability.snapshot().hydrated).toBe(true);
    // Past the notification service's two-minute duplicate window.
    at(5_200);
    replacement.receive([app.reminder("a", 1000)]);
    await flush();
    expect(app.show).toHaveBeenCalledOnce();
  } finally {
    await app.close();
  }
});

it("does not count a history read from before the drop as recovery", async () => {
  const app = await launch({ first: "history", banners: 1 });
  app.seed();
  at(5_000);
  try {
    await app.start();
    await app.nextPage();
    await vi.waitFor(() => expect(app.saved()).toBe("5000"));
    app.holdHistory(true);
    const prior = app.read();
    app.drop();
    at(6_000);
    app.answerHistory();
    await prior;
    app.poke();
    await flush();
    expect(app.reminders()?.status).toBe("loading");
    expect(app.saved()).toBe("5000");
  } finally {
    await app.close();
  }
});

it("keeps the saved time through a read that succeeds while offline, so a missed reminder notifies on relaunch", async () => {
  const app = await launch({ first: "history", banners: 1 });
  app.seed();
  at(5_000);
  try {
    await app.start();
    await app.nextPage();
    await vi.waitFor(() => expect(app.show).toHaveBeenCalledOnce());
    app.drop();
    at(6_000);
    // An HTTP read succeeds while the live connection keeps retrying.
    app.history.push(app.reminder("f", 6_500, 5_900));
    await app.read();
    expect(
      app.reminders()?.reminders.some((r) => r.id === "f".repeat(32)),
    ).toBe(true);
    expect(app.reminders()?.status).toBe("loading");
    // Another device adds a reminder due at 6200 that no live event delivers;
    // the timer for the known reminder must not save past it.
    await vi.advanceTimersByTimeAsync(500_000);
    expect(app.saved()).toBe("5000");
  } finally {
    await app.close();
  }
  at(7_000);
  const reopened = await launch({ first: "history", banners: 1, key: app.key });
  reopened.history.push(reopened.reminder("b", 6_200, 6_100));
  try {
    await reopened.start();
    await reopened.nextPage();
    // The reminder at 1000 is behind the saved 5000; only the missed one shows.
    await vi.waitFor(() => expect(reopened.show).toHaveBeenCalledOnce());
    await vi.waitFor(() => expect(reopened.saved()).toBe("7000"));
    expect(reopened.show).toHaveBeenCalledOnce();
  } finally {
    await reopened.close();
  }
});

it("issues a fresh read after reconnect instead of reusing one pending from the outage", async () => {
  const app = await launch({ first: "history", banners: 1 });
  app.seed();
  at(5_000);
  try {
    await app.start();
    await app.nextPage();
    await vi.waitFor(() => expect(app.saved()).toBe("5000"));
    app.drop();
    app.holdHistory(true);
    // This read takes its snapshot now; its response stays pending.
    const outageRead = app.read();
    app.history.push(app.reminder("b", 5_500, 5_400));
    at(6_000);
    await app.reconnect();
    await app.nextPage();
    // The outage read lands first and does not release the saved time.
    app.answerHistory(1);
    await outageRead;
    app.poke();
    await flush();
    expect(app.reminders()?.status).toBe("loading");
    expect(app.saved()).toBe("5000");
    // The read started after reconnect sees the new reminder and releases it.
    app.answerHistory();
    await vi.waitFor(() => expect(app.show).toHaveBeenCalledTimes(2));
    await vi.waitFor(() => expect(app.saved()).toBe("6000"));
  } finally {
    await app.close();
  }
});

it("keeps the saved time when the connection drops before its queued recovery runs", async () => {
  const app = await launch({ first: "history", banners: 1 });
  app.seed();
  at(5_000);
  app.history.push(app.reminder("f", 6_500, 4_900));
  try {
    await app.start();
    await app.nextPage();
    await vi.waitFor(() => expect(app.saved()).toBe("5000"));
    app.drop();
    at(6_000);
    app.establish();
    // The connection drops again before the queued recovery runs.
    app.drop();
    await app.nextPage();
    // A reminder due at 6200 from another device cannot arrive live; the timer
    // for the known one must not save past it.
    await vi.advanceTimersByTimeAsync(500_000);
    expect(app.liveStatus()).toBe("retrying");
    expect(app.reminders()?.status).toBe("loading");
    expect(app.saved()).toBe("5000");
  } finally {
    await app.close();
  }
  at(7_000);
  const reopened = await launch({ first: "history", banners: 1, key: app.key });
  reopened.history.push(reopened.reminder("b", 6_200, 6_100));
  try {
    await reopened.start();
    await reopened.nextPage();
    await vi.waitFor(() => expect(reopened.show).toHaveBeenCalledOnce());
    await vi.waitFor(() => expect(reopened.saved()).toBe("7000"));
    expect(reopened.show).toHaveBeenCalledOnce();
  } finally {
    await reopened.close();
  }
});

it("does not let a queued recovery run for a connection that was replaced before it", async () => {
  const app = await launch({ first: "history", banners: 1 });
  app.seed();
  at(5_000);
  try {
    await app.start();
    await app.nextPage();
    await vi.waitFor(() => expect(app.saved()).toBe("5000"));
    app.drop();
    at(6_000);
    app.establish();
    // Dropped and connected again, but the new subscription is not yet
    // established, so the first connection's queued recovery must not run.
    app.drop();
    app.poke();
    await app.nextPage();
    expect(app.reminders()?.status).toBe("loading");
    expect(app.saved()).toBe("5000");
  } finally {
    await app.close();
  }
});

it("never saves before the first live connection, while still loading the list", async () => {
  const app = await launch({ first: "history", banners: 1, offline: true });
  at(5_000);
  app.history.push(app.reminder("f", 6_500, 4_900));
  try {
    await app.start();
    await app.nextPage();
    // The HTTP read loads the list even though saving stays fenced.
    expect(app.reminders()).toMatchObject({
      status: "loading",
      hydrated: true,
    });
    expect(app.reminders()?.reminders).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(1_500_000);
    // The reminder due after the first bind still notifies in this session.
    expect(app.show).toHaveBeenCalledOnce();
    expect(app.liveStatus()).toBe("retrying");
    expect(app.saved()).toBe("5000");
  } finally {
    await app.close();
  }
  at(7_000);
  const reopened = await launch({ first: "history", banners: 1, key: app.key });
  reopened.history.push(reopened.reminder("b", 6_200, 6_100));
  try {
    await reopened.start();
    await reopened.nextPage();
    await vi.waitFor(() => expect(reopened.show).toHaveBeenCalledOnce());
    await vi.waitFor(() => expect(reopened.saved()).toBe("7000"));
    expect(reopened.show).toHaveBeenCalledOnce();
  } finally {
    await reopened.close();
  }
});

it("releases the saved time after the first connection's recovery read", async () => {
  const app = await launch({ first: "history", banners: 1, offline: true });
  app.seed();
  at(5_000);
  try {
    await app.start();
    await app.nextPage();
    await vi.waitFor(() => expect(app.show).toHaveBeenCalledOnce());
    expect(app.saved()).toBe("100");
    app.establish();
    await app.nextPage();
    await vi.waitFor(() => expect(app.saved()).toBe("5000"));
    expect(app.reminders()?.status).toBe("ready");
    expect(app.show).toHaveBeenCalledOnce();
  } finally {
    await app.close();
  }
});

it("does not run a recovery queued while the connection was not up", async () => {
  const app = await launch({ first: "history", banners: 1, offline: true });
  app.seed();
  at(5_000);
  try {
    await app.start();
    await app.nextPage();
    // An establishment reported while not connected queues a recovery that
    // must not run; connecting later without establishing proves nothing.
    app.announce();
    await flush();
    app.poke();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(app.reminders()?.status).toBe("loading");
    expect(app.saved()).toBe("100");
  } finally {
    await app.close();
  }
});

it("does not save from a ready list while the live connection is down", async () => {
  stored.set(`buzz-reminders-checked.v1:origin:${viewer}`, "100");
  const model = session(Promise.resolve([wire("later", 10_000)]));
  liveStatus = "retrying";
  const { connect } = mount();
  connect(`origin:${viewer}`, model.capability);
  at(200);
  await vi.advanceTimersByTimeAsync(1_000);
  expect(model.capability.snapshot().status).toBe("ready");
  expect(stored.get(`buzz-reminders-checked.v1:origin:${viewer}`)).toBe("100");
  liveStatus = "connected";
  model.receive([wire("other", 20_000)]);
  await vi.advanceTimersByTimeAsync(1_000);
  expect(stored.get(`buzz-reminders-checked.v1:origin:${viewer}`)).not.toBe(
    "100",
  );
});

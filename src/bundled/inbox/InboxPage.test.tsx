// @vitest-environment jsdom
import "@testing-library/jest-dom/vitest";
import { StrictMode } from "react";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import userEvent from "@testing-library/user-event";
import { InboxPage } from "./InboxPage";
import { composerDOMFixture } from "../../features/messages/composer-testing";
import { createRelaySession } from "../../features/relay/session";
import type { RelaySnapshot, RelayData } from "../../features/relay/service";
import type { Navigation } from "../../features/navigation/controller";
import type { ReadFilter, RelayEvent } from "../../features/relay/events";
import { matchesEvent } from "../../features/relay/projection";
import {
  readJournal,
  newReadJournal,
  type ReadJournal,
} from "../../features/relay/read-state-storage";
import {
  bounds,
  keypair,
  message,
  metadata,
  profile,
  roster,
  signed,
} from "../../features/relay/testing";
import {
  decodeReadState,
  signReadState,
  // @ts-expect-error Node host codec, with disposable test identities only.
} from "../../../browser-host/read-state.mjs";

const owners: ReturnType<typeof createRelaySession>[] = [];
composerDOMFixture();
beforeEach(() => localStorage.clear());
vi.stubGlobal(
  "ResizeObserver",
  class {
    observe() {}
    disconnect() {}
  },
);
HTMLElement.prototype.scrollIntoView = vi.fn();
afterEach(() => {
  cleanup();
  for (const owner of owners.splice(0)) owner.dispose();
  vi.useRealTimers();
});
function fixture(
  options: {
    failRoster?: boolean;
    archivedChannel?: boolean;
    withDm?: boolean;
    withSenders?: boolean;
    holdProfiles?: boolean;
    withWriter?: boolean;
    sessionChannel?: boolean;
    memberAgents?: 1 | 2;
    readCapability?: "read-only" | "unsupported";
  } = {},
) {
  const viewer = keypair(),
    alice = keypair(),
    agent = keypair(),
    profileAgent = keypair(),
    late = keypair(),
    unknown = keypair(),
    relayKey = keypair();
  let releaseProfiles = () => {};
  const profilesGate = options.holdProfiles
    ? new Promise<void>((resolve) => {
        releaseProfiles = resolve;
      })
    : undefined;
  let rosterFailure = options.failRoster ?? false;
  let rosterGate: Promise<void> | undefined;
  let rosterStarted = false;
  let evidenceReads = 0;
  let unreadGate: Promise<void> | undefined;
  let unreadStarted = false;
  const historyRequests: string[] = [];
  const threadRequests: string[] = [];
  const exactRequests: string[] = [];
  const published: RelayEvent[] = [];
  const historyGates = new Map<string, Promise<void>>();
  let addressedGate: Promise<void> | undefined;
  let releaseAddressed = () => {};
  let failAux = false;
  let auxGate: Promise<void> | undefined;
  let releaseAux = () => {};
  const historyFailures = new Set<string>();
  const answer = (filter: ReadFilter): RelayEvent[] => {
    const result = events.filter((event) => matchesEvent(event, filter));
    if (filter.depth_limit) {
      return result
        .filter(
          (event) =>
            filter.thread_cursor === undefined ||
            event.created_at > filter.thread_cursor ||
            (event.created_at === filter.thread_cursor &&
              event.id > (filter.thread_cursor_id ?? "")),
        )
        .sort((a, b) => a.created_at - b.created_at || a.id.localeCompare(b.id))
        .slice(0, filter.limit);
    }
    if (filter.top_level && filter["#h"]?.[0]) {
      return [
        ...result,
        ...events.filter(
          (event) =>
            event.kind === 5 &&
            event.tags.some(
              ([key, value]) => key === "h" && value === filter["#h"]?.[0],
            ),
        ),
        bounds(
          relayKey,
          filter["#h"][0],
          filter.until === undefined
            ? "head"
            : `${filter.until}:${filter.before_id}`,
          { has_more: false, next_cursor: null },
        ),
      ];
    }
    // The ordinary #p feed has no implicit include_aux. Exact #e pages
    // terminate only on empty even if visibility shortened an earlier page.
    if (filter["#e"])
      return result
        .filter(
          (event) =>
            filter.until === undefined ||
            event.created_at < filter.until ||
            (event.created_at === filter.until &&
              event.id > (filter.before_id ?? "")),
        )
        .slice(0, filter.limit);
    return result.slice(0, filter.limit);
  };
  let journal: ReadJournal | undefined;
  let saveFailure = false;
  let failThreadSave = false;
  let hold: Promise<void> | undefined;
  let saveStarted = false;
  let emit: (events: readonly RelayEvent[]) => void = () => {};
  const roots = [message(viewer, "room", "Our discussion", 20)];
  const mention = message(alice, "room", "Please review **this**", 21, [
    ["p", viewer.pubkey],
  ]);
  const reply = message(alice, "room", "A thread update", 22, [
    ["e", roots[0]?.id ?? "", "", "reply"],
  ]);
  const events = [
    roster(
      relayKey,
      "room",
      [
        viewer.pubkey,
        alice.pubkey,
        ...(options.memberAgents ? [agent.pubkey] : []),
        ...(options.memberAgents === 2 ? [profileAgent.pubkey] : []),
      ],
      10,
    ),
    metadata(
      relayKey,
      "room",
      "Design",
      10,
      options.sessionChannel
        ? [
            ["t", "stream"],
            ["private"],
            ["about", "Buzz session (buzz.sessions/v1)"],
          ]
        : [
            ["t", "stream"],
            ...(options.archivedChannel ? [["archived", "true"]] : []),
          ],
    ),
    profile(alice, { name: "Alice" }),
    ...roots,
    mention,
    reply,
    ...(options.withDm
      ? [
          roster(relayKey, "dm-room", [viewer.pubkey, alice.pubkey], 10),
          metadata(relayKey, "dm-room", "Direct message", 10, [
            ["t", "dm"],
            ["hidden"],
          ]),
          message(alice, "dm-room", "A direct reply", 23),
        ]
      : []),
    ...(options.withSenders
      ? [
          roster(relayKey, "agent-dm", [viewer.pubkey, agent.pubkey], 10),
          metadata(relayKey, "agent-dm", "Agent direct", 10, [
            ["t", "dm"],
            ["hidden"],
          ]),
          message(agent, "agent-dm", "Agent direct reply", 24),
          message(agent, "room", "Agent mention", 25, [["p", viewer.pubkey]]),
          message(unknown, "room", "Unprofiled mention", 26, [
            ["p", viewer.pubkey],
          ]),
          profile(profileAgent, { name: "Public agent", is_agent: true }),
          message(profileAgent, "room", "Public agent mention", 27, [
            ["p", viewer.pubkey],
          ]),
          message(late, "room", "Late profile mention", 28, [
            ["p", viewer.pubkey],
          ]),
        ]
      : []),
  ];
  if (options.readCapability) {
    journal = {
      ...newReadJournal(),
      state: {
        frontiers: { [`msg:${mention.id}`]: mention.created_at },
        overrides: {},
      },
      localUnread: { [`msg:${"f".repeat(64)}`]: 1 },
      revision: 1,
      acceptedRevision: 1,
    };
  }
  const owner = createRelaySession(
    {
      viewer: viewer.pubkey,
      relayAuthor: relayKey.pubkey,
      query: async (filters) => {
        if (
          filters.some(
            (filter) =>
              filter.include_aux && !!filter["#h"] && filter.kinds?.includes(9),
          )
        ) {
          unreadStarted = true;
          await unreadGate;
        }
        if (
          filters.some((filter) => !!filter["#p"] && filter.kinds?.includes(9))
        )
          await addressedGate;
        if (
          filters.some(
            (filter) => !!filter["#e"] && filter.kinds?.includes(40003),
          )
        ) {
          await auxGate;
          if (failAux) throw new Error("auxiliary history unavailable");
        }
        if (profilesGate && filters.some((filter) => filter.kinds?.includes(0)))
          await profilesGate;
        if (filters.some((filter) => filter.kinds?.includes(39002))) {
          rosterStarted = true;
          if (rosterGate) await rosterGate;
          if (rosterFailure) throw new Error("roster offline");
        }
        if (filters.some((filter) => filter.kinds?.includes(9)))
          evidenceReads++;
        for (const filter of filters) {
          const channelId = filter["#h"]?.[0];
          if (filter.top_level && channelId) historyRequests.push(channelId);
          if (filter.depth_limit && channelId) threadRequests.push(channelId);
          if (filter.ids && channelId) exactRequests.push(...filter.ids);
          if (
            (filter.top_level || filter.depth_limit || filter.ids) &&
            channelId
          ) {
            await historyGates.get(channelId);
            if (historyFailures.has(channelId))
              throw new Error("history offline");
          }
        }
        return filters.flatMap(answer);
      },
      media: () => undefined,
      ...(options.withWriter
        ? {
            writer: {
              kinds: [9, 9007],
              sign: async (template: import("nostr-tools").EventTemplate) =>
                signed(viewer, template),
              publish: async (event: RelayEvent) => {
                published.push(event);
              },
            },
          }
        : {}),
      ...(options.withSenders || options.memberAgents
        ? {
            readAgentLibrary: async () => ({
              definitions: [],
              identities: [
                { pubkey: agent.pubkey, name: "Scout" },
                ...(options.memberAgents === 2
                  ? [{ pubkey: profileAgent.pubkey, name: "Second agent" }]
                  : []),
              ],
            }),
          }
        : {}),
      subscribe(callbacks) {
        if (options.memberAgents)
          callbacks.state({ status: "connected", routes: [] });
        emit = callbacks.receive;
        return { update() {}, retry() {}, dispose() {} };
      },
      ...(options.readCapability === "unsupported"
        ? {}
        : {
            readState: {
              decode: async (records: readonly RelayEvent[]) =>
                decodeReadState(records, viewer.secret),
              ...(options.readCapability === "read-only"
                ? {}
                : {
                    sign: async (
                      intent: import("../../features/relay/read-state-host").ReadStateSigning,
                    ) => signReadState(intent, viewer.secret),
                    publish: async (event: RelayEvent) => {
                      events.push(event);
                    },
                  }),
            },
          }),
    },
    {
      outboxStorage: { load: () => [], save: () => {} },
      readStateStorage: {
        async update(change) {
          if (hold) {
            saveStarted = true;
            const wait = hold;
            hold = undefined;
            await wait;
          }
          if (saveFailure) {
            saveFailure = false;
            throw new Error("disk full");
          }
          const next = readJournal(change(journal), viewer.pubkey);
          if (
            failThreadSave &&
            Object.keys(next.state.frontiers).some(
              (key) =>
                key.startsWith("thread:") &&
                next.state.frontiers[key] !== journal?.state.frontiers[key],
            )
          ) {
            failThreadSave = false;
            throw new Error("thread disk full");
          }
          journal = next;
          return journal;
        },
        close() {},
      },
      readPublisherLock: async (_signal, work) => work(),
    },
  );
  owners.push(owner);
  if (!options.failRoster) emit(events);
  const scope = {
    viewer: viewer.pubkey,
    communityOrigin: "https://relay.test",
  };
  const readSteps: {
    target: Parameters<typeof owner.session.unread.markThrough>[0];
    id: string;
    work: ReturnType<typeof owner.session.unread.markThrough>;
  }[] = [];
  const retrySync = vi.fn(() => owner.session.unread.retrySync());
  const observedSession = {
    ...owner.session,
    unread: {
      ...owner.session.unread,
      retrySync,
      markThrough(
        target: Parameters<typeof owner.session.unread.markThrough>[0],
        id: string,
      ) {
        const work = owner.session.unread.markThrough(target, id);
        readSteps.push({ target, id, work });
        return work;
      },
    },
  };
  let connection: RelaySnapshot = {
    status: "ready",
    generation: 1,
    scope: `${scope.communityOrigin}:${scope.viewer}`,
    viewer: scope.viewer,
    session: observedSession,
  };
  const subscribers = new Set<() => void>();
  const relay: RelayData = {
    snapshot: () => connection,
    subscribe(listener) {
      subscribers.add(listener);
      return () => subscribers.delete(listener);
    },
    retry() {},
    disconnect() {},
    clearCache: owner.clearCache,
  };
  const open = vi.fn<Navigation["open"]>(async () => ({ status: "opened" }));
  return {
    owner,
    relay,
    retrySync,
    readSteps,
    evidenceReads: () => evidenceReads,
    historyRequests,
    unreadStarted: () => unreadStarted,
    holdUnread() {
      unreadStarted = false;
      let release = () => {};
      unreadGate = new Promise<void>((resolve) => {
        release = resolve;
      });
      return () => {
        unreadGate = undefined;
        release();
      };
    },
    holdAddressed() {
      addressedGate = new Promise<void>((resolve) => {
        releaseAddressed = resolve;
      });
      return () => {
        addressedGate = undefined;
        releaseAddressed();
      };
    },
    holdAux() {
      auxGate = new Promise<void>((resolve) => {
        releaseAux = resolve;
      });
      return () => {
        auxGate = undefined;
        releaseAux();
      };
    },
    failAux(value = true) {
      failAux = value;
    },
    threadRequests,
    exactRequests,
    published,
    addEvent(event: RelayEvent) {
      events.push(event);
    },
    deleteHistory(channelId: string) {
      const deletions = events
        .filter(
          (e) =>
            e.kind === 9 &&
            e.tags.some(([k, v]) => k === "h" && v === channelId),
        )
        .map((event) =>
          signed(event.pubkey === viewer.pubkey ? viewer : alice, {
            kind: 5,
            tags: [
              ["h", channelId],
              ["e", event.id],
            ],
            content: "",
            created_at: 200,
          }),
        );
      events.push(...deletions);
      emit(deletions);
    },
    retireThreadRoot() {
      const root = roots[0];
      if (!root) throw new Error("Missing fixture root");
      const deletion = signed(viewer, {
        kind: 5,
        tags: [
          ["h", "room"],
          ["e", root.id],
        ],
        content: "",
        created_at: 200,
      });
      events.push(deletion);
      emit([deletion]);
    },
    root: roots[0],
    failHistory(channelId: string) {
      historyFailures.add(channelId);
    },
    recoverHistory(channelId: string) {
      historyFailures.delete(channelId);
    },
    emptyHistory(channelId: string) {
      for (let i = events.length - 1; i >= 0; i--)
        if (
          events[i]?.kind === 9 &&
          events[i]?.tags.some(([k, v]) => k === "h" && v === channelId)
        )
          events.splice(i, 1);
    },
    holdHistory(channelId: string) {
      let release = () => {};
      historyGates.set(
        channelId,
        new Promise<void>((resolve) => {
          release = resolve;
        }),
      );
      return () => {
        historyGates.delete(channelId);
        release();
      };
    },
    rosterStarted: () => rosterStarted,
    releaseProfiles,
    agent,
    unknown,
    late,
    alice,
    viewer,
    profileAgent,
    emit,
    events,
    publishLateProfile() {
      act(() => emit([profile(late, { name: "Late person" }, 30)]));
    },
    publishAgentProfile() {
      act(() => emit([profile(agent, { name: "Scout", is_agent: true }, 27)]));
    },
    publishMalformedProfile() {
      act(() =>
        emit([
          signed(unknown, {
            kind: 0,
            tags: [],
            content: "broken",
            created_at: 27,
          }),
        ]),
      );
    },
    recoverRoster() {
      rosterFailure = false;
      rosterStarted = false;
      let release = () => {};
      rosterGate = new Promise<void>((resolve) => {
        release = resolve;
      });
      return release;
    },
    scope,
    mention,
    reply,
    open,
    journal: () => journal,
    view: (
      <StrictMode>
        <InboxPage
          relay={relay}
          navigator={{ open } as unknown as Navigation}
        />
      </StrictMode>
    ),
    revokeRoom() {
      emit([roster(relayKey, "room", [alice.pubkey], 100)]);
    },
    restoreRoom() {
      const restored = roster(
        relayKey,
        "room",
        [viewer.pubkey, alice.pubkey],
        101,
      );
      events.push(restored);
      emit([restored, mention, reply, ...roots]);
    },
    archiveRoom(archived: boolean, time: number) {
      const state = metadata(relayKey, "room", "Design", time, [
        ["t", "stream"],
        ["archived", String(archived)],
      ]);
      events.push(state);
      emit([state]);
    },
    renameRoom() {
      const renamed = metadata(relayKey, "room", "Renamed", 99, [
        ["t", "stream"],
      ]);
      events.push(renamed);
      emit([renamed]);
    },
    failSave() {
      saveFailure = true;
    },
    failThreadSave() {
      failThreadSave = true;
    },
    saveStarted: () => saveStarted,
    holdSave() {
      saveStarted = false;
      let release = () => {};
      hold = new Promise<void>((resolve) => {
        release = resolve;
      });
      return release;
    },
    disconnect() {
      connection = { ...connection, status: "disconnected", generation: 2 };
      for (const listener of subscribers) listener();
    },
  };
}
const rows = () =>
  within(
    screen.getByRole("list", { name: "Inbox conversations" }),
  ).queryAllByRole("listitem");
async function chooseFilter(label: string, control = "Activity type") {
  const user = userEvent.setup();
  const trigger = screen.getByRole("button", { name: "Inbox filters" });
  await user.click(trigger);
  await screen.findByRole("menu", { name: "Inbox filters" });
  if (control === "Sender") {
    const people = screen.getByRole("menuitemcheckbox", { name: "People" });
    const agents = screen.getByRole("menuitemcheckbox", { name: "Agents" });
    // Enable the desired sender before disabling the other: never select none.
    if (label !== "Agents" && people.getAttribute("aria-checked") !== "true")
      await user.click(people);
    if (label !== "Humans" && agents.getAttribute("aria-checked") !== "true")
      await user.click(agents);
    if (label === "Agents" && people.getAttribute("aria-checked") === "true")
      await user.click(people);
    if (label === "Humans" && agents.getAttribute("aria-checked") === "true")
      await user.click(agents);
    await user.keyboard("{Escape}");
  } else {
    await user.click(
      screen.getByRole("menuitemradio", {
        name: label === "All activity" ? "All" : label,
      }),
    );
  }
  await waitFor(() =>
    expect(
      screen.queryByRole("menu", { name: "Inbox filters" }),
    ).not.toBeInTheDocument(),
  );
}
async function openRowMenu(
  method: "context" | "keyboard" | "contextKey" = "context",
) {
  const row = rows()[0];
  if (!row) throw new Error("Missing inbox row");
  if (method === "context")
    fireEvent.contextMenu(within(row).getByRole("button", { name: /^Open / }), {
      clientX: 20,
      clientY: 20,
    });
  else {
    const open = within(row).getByRole("button", { name: /^Open / });
    open.focus();
    fireEvent.keyDown(
      open,
      method === "keyboard"
        ? { key: "F10", shiftKey: true }
        : { key: "ContextMenu" },
    );
  }
  return screen.findByRole("menuitem", { name: "Mark unread" });
}
it("excludes archived-channel mentions and threads without hiding active DMs", async () => {
  const h = fixture({ archivedChannel: true, withDm: true });
  render(h.view);
  await screen.findByText("A direct reply");
  await waitFor(() =>
    expect(
      screen.queryByText("Checking recent activity…"),
    ).not.toBeInTheDocument(),
  );
  expect(rows()).toHaveLength(1);
  expect(rows()[0]).toHaveTextContent("A direct reply");
  await chooseFilter("Mentions");
  expect(rows()).toHaveLength(0);
  await chooseFilter("Threads");
  expect(rows()).toHaveLength(0);
});

it.each(["Please review", "A thread update"])(
  "closes selected Inbox detail on channel archive and restores unread intent on unarchive (%s)",
  async (preview) => {
    const h = fixture({ withWriter: true });
    render(h.view);
    await waitFor(() => expect(rows()).toHaveLength(2));
    const row = rows().find((item) => item.textContent?.includes(preview));
    if (!row) throw new Error("Missing fixture conversation");
    fireEvent.click(within(row).getByRole("button", { name: /^Open / }));
    const detail = screen.getByRole("region", { name: "Inbox detail" });
    await within(detail).findByText(
      preview === "Please review" ? /Please review/ : "A thread update",
    );
    await waitFor(() =>
      expect(
        within(row).queryByRole("img", { name: "Unread" }),
      ).not.toBeInTheDocument(),
    );
    fireEvent.contextMenu(within(row).getByRole("button", { name: /^Open / }));
    fireEvent.click(
      await screen.findByRole("menuitem", { name: "Mark unread" }),
    );
    await waitFor(() =>
      expect(
        within(row).getByRole("img", { name: "Unread" }),
      ).toBeInTheDocument(),
    );
    const journal = h.journal();
    act(() => h.archiveRoom(true, 100));
    await waitFor(() => expect(rows()).toHaveLength(0));
    expect(
      screen.queryByRole("region", { name: "Inbox detail" }),
    ).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Inbox filters" })).toHaveFocus();
    expect(h.journal()).toEqual(journal);
    act(() => h.archiveRoom(false, 101));
    await waitFor(() => expect(rows()).toHaveLength(2));
    const restored = rows().find((item) => item.textContent?.includes(preview));
    if (!restored) throw new Error("Missing restored conversation");
    expect(
      within(restored).getByRole("img", { name: "Unread" }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("region", { name: "Inbox detail" }),
    ).not.toBeInTheDocument();
    expect(h.journal()).toEqual(journal);
  },
);

it.each([false, true])(
  "retires selected visits only on explicit archive during a held unread refresh (archive=%s)",
  async (archive) => {
    const h = fixture();
    render(h.view);
    await screen.findByText("Please review this");
    await waitFor(() =>
      expect(h.owner.session.unread.inbox().status).toBe("ready"),
    );
    await chooseFilter("Mentions");
    fireEvent.click(
      screen.getByRole("button", { name: "Open Alice in #Design" }),
    );
    await within(
      screen.getByRole("region", { name: "Inbox detail" }),
    ).findByText(/Please review/);
    await waitFor(() =>
      expect(
        screen.queryByRole("img", { name: "Unread" }),
      ).not.toBeInTheDocument(),
    );
    const journal = h.journal();
    const release = h.holdUnread();
    let refresh: Promise<void> | undefined;
    try {
      act(() => {
        refresh = h.owner.session.unread.refresh();
      });
      await waitFor(() => expect(h.unreadStarted()).toBe(true));
      expect(h.owner.session.unread.inbox().status).toBe("loading");
      expect(
        screen.getByRole("region", { name: "Inbox detail" }),
      ).toBeInTheDocument();
      if (archive) {
        act(() => h.archiveRoom(true, 100));
        await waitFor(() => expect(rows()).toHaveLength(0));
        expect(
          screen.queryByRole("region", { name: "Inbox detail" }),
        ).not.toBeInTheDocument();
        act(() => h.archiveRoom(false, 101));
        await waitFor(() => expect(rows()).toHaveLength(1));
        expect(h.owner.session.unread.inbox().status).toBe("loading");
        expect(
          screen.queryByRole("region", { name: "Inbox detail" }),
        ).not.toBeInTheDocument();
        expect(
          screen.getByRole("button", { name: "Inbox filters" }),
        ).toHaveFocus();
        expect(h.journal()).toEqual(journal);
      }
    } finally {
      await act(async () => {
        release();
        await refresh;
      });
    }
    await waitFor(() =>
      expect(h.owner.session.unread.inbox().status).toBe("ready"),
    );
    expect(!!screen.queryByRole("region", { name: "Inbox detail" })).toBe(
      !archive,
    );
    expect(h.journal()).toEqual(journal);
  },
);

it("retires a failed captured read on archive during a held unread refresh", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await waitFor(() =>
    expect(h.owner.session.unread.inbox().status).toBe("ready"),
  );
  await chooseFilter("Mentions");
  h.failSave();
  fireEvent.click(
    screen.getByRole("button", { name: "Open Alice in #Design" }),
  );
  expect(await screen.findByRole("alert")).toHaveTextContent("disk full");
  const release = h.holdUnread();
  let refresh: Promise<void> | undefined;
  try {
    act(() => {
      refresh = h.owner.session.unread.refresh();
    });
    await waitFor(() => expect(h.unreadStarted()).toBe(true));
    act(() => h.archiveRoom(true, 100));
    await waitFor(() => expect(rows()).toHaveLength(0));
    act(() => h.archiveRoom(false, 101));
    await waitFor(() => expect(rows()).toHaveLength(1));
    expect(h.owner.session.unread.inbox().status).toBe("loading");
    expect(
      screen.queryByRole("button", { name: "Retry inbox" }),
    ).not.toBeInTheDocument();
  } finally {
    await act(async () => {
      release();
      await refresh;
    });
  }
  await waitFor(() =>
    expect(h.owner.session.unread.inbox().status).toBe("ready"),
  );
  expect(
    screen.queryByRole("region", { name: "Inbox detail" }),
  ).not.toBeInTheDocument();
  expect(h.journal()?.state.frontiers[`msg:${h.mention.id}`]).toBeUndefined();
  expect(screen.getByRole("img", { name: "Unread" })).toBeInTheDocument();
});

it("filters real session evidence, opens an exact message and marks it read, and shares durable local unread", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await waitFor(() =>
    expect(
      screen.queryByText("Checking recent activity…"),
    ).not.toBeInTheDocument(),
  );
  expect(rows()).toHaveLength(2);
  await chooseFilter("Mentions");
  expect(rows()).toHaveLength(1);
  fireEvent.click(
    await screen.findByRole("button", { name: "Open Alice in #Design" }),
  );
  expect(
    await screen.findByRole("region", { name: "Inbox detail" }),
  ).toBeInTheDocument();
  expect(h.open).not.toHaveBeenCalled();
  expect(
    within(screen.getByRole("heading", { name: "#Design" })).getByRole(
      "button",
      { name: "#Design" },
    ),
  ).toHaveAttribute("title", "Open full thread");
  fireEvent.click(screen.getByRole("button", { name: "Open in channel" }));
  await waitFor(() =>
    expect(h.open).toHaveBeenCalledWith({
      version: 1,
      kind: "conversation",
      scope: h.scope,
      channelId: "room",
      messageId: h.mention.id,
    }),
  );
  await waitFor(() =>
    expect(
      screen.queryByRole("img", { name: "Unread" }),
    ).not.toBeInTheDocument(),
  );
  expect(h.journal()?.state.frontiers[`msg:${h.mention.id}`]).toBe(21);
  expect(
    h.owner.session.unread.snapshot({ kind: "channel", channelId: "room" })
      .observedCount,
  ).toBe(1);
  fireEvent.click(await openRowMenu("keyboard"));
  await waitFor(() =>
    expect(screen.getByRole("img", { name: "Unread" })).toBeInTheDocument(),
  );
  fireEvent.click(
    screen.getByRole("button", { name: "Open Alice in #Design" }),
  );
  expect(screen.getByRole("img", { name: "Unread" })).toBeInTheDocument();
  expect(
    screen.queryByText(/Marked unread on this device/),
  ).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Unread only" }));
  expect(rows()).toHaveLength(1);
  await chooseFilter("Threads");
  expect(rows()).toHaveLength(1);
  expect(screen.getByText("A thread update")).toBeInTheDocument();
});

it("puts the actual channel or DM source below the sender without category tags", async () => {
  const h = fixture({ withDm: true });
  render(h.view);
  await screen.findByText("A direct reply");
  await waitFor(() => expect(rows()).toHaveLength(3));
  const dmRow = rows().find((row) =>
    row.textContent?.includes("A direct reply"),
  );
  const mentionRow = rows().find((row) =>
    row.textContent?.includes("Please review this"),
  );
  if (!dmRow || !mentionRow) throw new Error("Expected DM and channel rows");
  const dmSource = dmRow.querySelector("[data-inbox-source]");
  const channelSource = mentionRow.querySelector("[data-inbox-source]");
  expect(dmSource).toHaveTextContent("DM · Alice");
  expect(channelSource).toHaveTextContent("#Design");
  expect(
    within(dmRow).getByText("Alice", { selector: "strong" }),
  ).toBeInTheDocument();
  expect(
    within(mentionRow).getByText("Alice", { selector: "strong" }),
  ).toBeInTheDocument();
  expect(dmSource).toHaveClass(/source/);
  expect(channelSource).toHaveClass(/source/);
  expect(
    (dmSource?.compareDocumentPosition(
      within(dmRow).getByText("A direct reply"),
    ) ?? 0) & Node.DOCUMENT_POSITION_FOLLOWING,
  ).toBeTruthy();
  expect(
    (channelSource?.compareDocumentPosition(
      within(mentionRow).getByText("Please review this"),
    ) ?? 0) & Node.DOCUMENT_POSITION_FOLLOWING,
  ).toBeTruthy();
  for (const row of [dmRow, mentionRow]) {
    expect(row).not.toHaveTextContent(
      / · (Mention|Thread|Agent)|Needs action|Project update/,
    );
  }
  expect(
    within(dmRow).getByRole("button", { name: "Open Alice in DM · Alice" }),
  ).toBeInTheDocument();
  await chooseFilter("DMs");
  expect(rows()).toHaveLength(1);
  expect(rows()[0]).toHaveTextContent("A direct reply");
  await chooseFilter("Mentions");
  expect(rows()).toHaveLength(1);
  expect(rows()[0]).toHaveTextContent("Please review this");
  await chooseFilter("All activity");
  expect(rows()).toHaveLength(3);
});

it("distinguishes same-sender thread choices by their visible safe preview", async () => {
  const h = fixture();
  const root = message(h.viewer, "room", "Another discussion", 30);
  const reply = message(h.alice, "room", "A **different** thread update", 31, [
    ["e", root.id, "", "reply"],
  ]);
  h.events.push(root, reply);
  h.emit([root, reply]);
  render(h.view);
  await screen.findByText("A different thread update");
  await chooseFilter("Threads");
  expect(rows()).toHaveLength(2);
  for (const preview of ["A thread update", "A different thread update"]) {
    const choice = screen.getByRole("button", {
      name: "Open Alice in #Design",
      description: preview,
    });
    expect(choice).toContainElement(screen.getByText(preview));
  }
});

it("renders all loaded rows without pagination while preserving unread filtering", async () => {
  const h = fixture();
  const extra = Array.from({ length: 49 }, (_, index) =>
    message(h.alice, "room", `Additional mention ${index}`, 100 + index, [
      ["p", h.viewer.pubkey],
    ]),
  );
  h.events.push(...extra);
  h.emit(extra);
  render(h.view);
  await screen.findByText("Additional mention 48");
  await waitFor(() =>
    expect(
      screen.queryByText("Checking recent activity…"),
    ).not.toBeInTheDocument(),
  );
  expect(rows()).toHaveLength(51);
  expect(
    screen.queryByRole("button", { name: "Show more" }),
  ).not.toBeInTheDocument();
  await act(async () => {
    await h.owner.session.unread.markChannelRead("room");
  });
  fireEvent.click(screen.getByRole("button", { name: "Unread only" }));
  expect(rows()).toHaveLength(0);
  expect(
    screen.queryByRole("button", { name: "Show more" }),
  ).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Unread only" }));
  expect(rows()).toHaveLength(51);
  expect(screen.queryByRole("button", { name: "Show more" })).toBeNull();
});

it("intersects activity and representative sender evidence without inferring missing profiles", async () => {
  const h = fixture({ withDm: true, withSenders: true, holdProfiles: true });
  render(h.view);
  await screen.findByText("Agent direct reply");
  expect(
    screen.getByRole("button", { name: "Inbox filters" }),
  ).toHaveTextContent("All");
  await chooseFilter("DMs");
  await chooseFilter("Agents", "Sender");
  // Inbox's idle-only ensure reads the shared choices; no filter-owned inventory.
  expect(rows()).toHaveLength(1);
  expect(rows()[0]).toHaveTextContent("Agent direct reply");
  await chooseFilter("Mentions");
  expect(rows()).toHaveLength(1);
  expect(rows()[0]).toHaveTextContent("Agent mention");
  await chooseFilter("Humans", "Sender");
  expect(rows()).toHaveLength(0);
  try {
    await act(async () => h.releaseProfiles());
    await waitFor(() => expect(rows()).toHaveLength(1));
    expect(rows()[0]).toHaveTextContent("Please review this");
    await chooseFilter("Agents", "Sender");
    h.publishAgentProfile();
    await waitFor(() => expect(rows()).toHaveLength(2));
    expect(
      rows().some((row) => row.textContent?.includes("Public agent mention")),
    ).toBe(true);
    await chooseFilter("Humans", "Sender");
    h.publishMalformedProfile();
    h.publishLateProfile();
    await waitFor(() => expect(rows()).toHaveLength(2));
    expect(
      rows().some((row) => row.textContent?.includes("Late profile mention")),
    ).toBe(true);
    expect(
      rows().some((row) => row.textContent?.includes("Unprofiled mention")),
    ).toBe(false);
    await chooseFilter("Everyone", "Sender");
    await waitFor(() => expect(rows().length).toBeGreaterThan(4));
    expect(
      rows().some((row) => row.textContent?.includes("Unprofiled mention")),
    ).toBe(true);
    expect(h.journal()?.state.frontiers).toEqual({}); // filtering never marks read
  } finally {
    h.releaseProfiles();
  }
});

it("omits visible row overflow buttons and preserves disabled unread via right-click", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await chooseFilter("Mentions");
  const row = rows()[0];
  if (!row) throw new Error("Missing inbox row");
  expect(within(row).getAllByRole("button")).toHaveLength(1);
  expect(
    within(row).queryByRole("button", { name: /^Actions for / }),
  ).not.toBeInTheDocument();
  expect(row.querySelector("[data-inbox-row-overflow]")).toBeNull();
  expect(within(row).getByRole("img", { name: "Unread" })).toBeInTheDocument();
  const action = await openRowMenu("context");
  expect(action).toHaveAttribute("aria-disabled", "true");
  expect(
    screen.queryByRole("region", { name: "Inbox detail" }),
  ).not.toBeInTheDocument();
  expect(h.journal()?.state.frontiers).toEqual({});
});

it("opens local unread actions with the ContextMenu key without changing selection", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await chooseFilter("Mentions");
  fireEvent.click(
    screen.getByRole("button", { name: "Open Alice in #Design" }),
  );
  await waitFor(() =>
    expect(
      screen.queryByRole("img", { name: "Unread" }),
    ).not.toBeInTheDocument(),
  );
  fireEvent.click(screen.getByRole("button", { name: "Close detail" }));
  const action = await openRowMenu("contextKey");
  expect(action).toHaveTextContent("Mark unread");
  expect(h.journal()?.state.frontiers[`msg:${h.mention.id}`]).toBe(21);
  expect(
    screen.queryByRole("region", { name: "Inbox detail" }),
  ).not.toBeInTheDocument();
  fireEvent.keyDown(action, { key: "Escape" });
});

it("holds read actions pending, surfaces storage failure, and retries without losing evidence", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await chooseFilter("Mentions");
  const release = h.holdSave();
  h.failSave();
  try {
    fireEvent.click(
      screen.getByRole("button", { name: "Open Alice in #Design" }),
    );
    await waitFor(() =>
      expect(
        screen.getByRole("region", { name: "Inbox detail" }),
      ).toBeInTheDocument(),
    );
  } finally {
    await act(async () => release());
  }
  expect(await screen.findByRole("alert")).toHaveTextContent("disk full");
  expect(screen.getByText("Please review this")).toBeInTheDocument();
  expect(h.journal()?.state.frontiers[`msg:${h.mention.id}`]).toBeUndefined();
  fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
  await waitFor(() =>
    expect(
      screen.queryByRole("img", { name: "Unread" }),
    ).not.toBeInTheDocument(),
  );
  expect(h.journal()?.state.frontiers[`msg:${h.mention.id}`]).toBe(21);
});

it.each([false, true])(
  "a two-step Inbox read admits the second prefix only while mounted (retire=%s)",
  async (retire) => {
    const h = fixture();
    const root = message(h.alice, "room", "Two-step root", 30, [
      ["p", h.viewer.pubkey],
    ]);
    const child = message(h.alice, "room", "Two-step reply", 31, [
      ["p", h.viewer.pubkey],
      ["e", root.id, "", "reply"],
    ]);
    h.events.push(root, child);
    h.emit([root, child]);
    render(h.view);
    await screen.findByText("Two-step root");
    await waitFor(() =>
      expect(
        screen.queryByText("Checking recent activity…"),
      ).not.toBeInTheDocument(),
    );
    const row = h.owner.session.unread
      .inbox()
      .items.find((item) => item.id === `room:${root.id}`);
    if (!row) throw Error("Missing two-step conversation");
    expect(row.readThrough).toEqual([
      {
        target: { kind: "message", channelId: "room", messageId: root.id },
        messageId: root.id,
      },
      {
        target: { kind: "thread", channelId: "room", rootId: root.id },
        messageId: child.id,
      },
    ]);
    const release = h.holdSave();
    try {
      const target = rows().find((row) =>
        row.textContent?.includes("Two-step root"),
      );
      if (!target) throw Error("Missing two-step row");
      fireEvent.click(within(target).getByRole("button"));
      await waitFor(() => expect(h.saveStarted()).toBe(true));
      expect(h.readSteps).toHaveLength(1);
      expect(h.readSteps[0]?.id).toBe(root.id);
      if (retire) act(() => h.disconnect());
    } finally {
      await act(async () => release());
    }
    const first = h.readSteps[0];
    if (!first) throw Error("First read never started");
    await act(async () => {
      await first.work;
    });
    if (!retire) {
      await waitFor(() => expect(h.readSteps).toHaveLength(2));
      const second = h.readSteps[1];
      if (!second) throw Error("Second read never started");
      await act(async () => {
        await second.work;
      });
      expect(second.id).toBe(child.id);
      expect(h.journal()?.state.frontiers[`thread:${root.id}`]).toBe(31);
    } else {
      expect(h.readSteps).toHaveLength(1);
      expect(h.journal()?.state.frontiers[`thread:${root.id}`]).toBeUndefined();
      expect(
        screen.getByText("Choose a community to see your inbox."),
      ).toBeInTheDocument();
    }
    expect(h.journal()?.state.frontiers[`msg:${root.id}`]).toBe(30);
  },
);

it.each([
  ["Close", false, false],
  ["Escape", false, false],
  ["Close", true, false],
  ["Escape", true, false],
  ["access", false, true],
  ["Close", false, true],
] as const)(
  "two-step read %s during its first save (storage failure=%s, access loss=%s)",
  async (action, storageFailure, accessLoss) => {
    const h = fixture();
    const user = userEvent.setup();
    const root = message(h.alice, "room", "Cancelled root", 30, [
      ["p", h.viewer.pubkey],
    ]);
    const child = message(h.alice, "room", "Cancelled reply", 31, [
      ["p", h.viewer.pubkey],
      ["e", root.id, "", "reply"],
    ]);
    h.events.push(root, child);
    h.emit([root, child]);
    render(h.view);
    await screen.findByText("Cancelled root");
    await waitFor(() =>
      expect(h.owner.session.inboxFeed.snapshot().status).toBe("ready"),
    );
    expect(
      h.owner.session.unread
        .inbox()
        .items.find((item) => item.id === `room:${root.id}`)?.readThrough,
    ).toHaveLength(2);
    const release = h.holdSave();
    if (storageFailure) h.failSave();
    try {
      const row = rows().find((row) =>
        row.textContent?.includes("Cancelled root"),
      );
      if (!row) throw Error("Missing two-step row");
      await user.click(within(row).getByRole("button"));
      await waitFor(() => expect(h.saveStarted()).toBe(true));
      expect(h.readSteps.map((step) => step.id)).toEqual([root.id]);
      const close = screen.getByRole("button", { name: "Close detail" });
      if (action === "Close") await user.click(close);
      if (action === "Escape") {
        close.focus();
        await user.keyboard("{Escape}");
      }
      if (accessLoss) act(() => h.revokeRoom());
      expect(
        screen.queryByRole("region", { name: "Inbox detail" }),
      ).not.toBeInTheDocument();
    } finally {
      await act(async () => release());
    }
    await waitFor(() =>
      expect(
        screen.getByRole("list", {
          name: "Inbox conversations",
        }),
      ).toHaveAttribute("aria-busy", "false"),
    );
    expect(h.readSteps.map((step) => step.id)).toEqual([root.id]);
    expect(h.journal()?.state.frontiers[`thread:${root.id}`]).toBeUndefined();
    if (storageFailure || accessLoss) {
      expect(screen.getByRole("alert")).toHaveTextContent(
        storageFailure ? "disk full" : "Reading observation expired",
      );
    } else {
      expect(h.journal()?.state.frontiers[`msg:${root.id}`]).toBe(30);
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    }
  },
);

it.each(["retry", "other", "blur"] as const)(
  "held read Retry completion preserves focus ownership (%s)",
  async (focusOwner) => {
    const h = fixture();
    const user = userEvent.setup();
    render(h.view);
    await screen.findByText("Please review this");
    await chooseFilter("Mentions");
    h.failSave();
    await user.click(
      screen.getByRole("button", { name: "Open Alice in #Design" }),
    );
    expect(await screen.findByRole("alert")).toHaveTextContent("disk full");
    const detail = screen.getByRole("region", { name: "Inbox detail" });
    // The exact reader has already consumed its opening reveal, not a future focus handoff.
    const target = detail.querySelector(`[data-message-id="${h.mention.id}"]`);
    await waitFor(() => expect(target).toHaveFocus());
    const retry = screen.getByRole("button", { name: "Retry inbox" });
    const other = within(detail).getByRole("button", {
      name: "Open in channel",
    });
    const release = h.holdSave();
    try {
      retry.focus();
      await user.keyboard("{Enter}");
      await waitFor(() => expect(h.saveStarted()).toBe(true));
      expect(retry).toBeInTheDocument();
      expect(retry).toHaveFocus();
      expect(retry).not.toBeDisabled();
      expect(retry).toHaveAttribute("aria-disabled", "true");
      await user.keyboard("{Enter}");
      expect(h.readSteps).toHaveLength(2);
      if (focusOwner === "other") other.focus();
      if (focusOwner === "blur") retry.blur();
    } finally {
      await act(async () => release());
    }
    await waitFor(() =>
      expect(screen.queryByText("disk full")).not.toBeInTheDocument(),
    );
    expect(h.journal()?.state.frontiers[`msg:${h.mention.id}`]).toBe(21);
    if (focusOwner === "retry") {
      expect(
        within(detail).getByRole("button", { name: "Close detail" }),
      ).toHaveFocus();
      await user.keyboard("{Escape}");
      expect(detail).not.toBeInTheDocument();
    } else expect(focusOwner === "other" ? other : document.body).toHaveFocus();
  },
);

it("a second rejected Retry retains its focused control until a successful recovery", async () => {
  const h = fixture();
  const user = userEvent.setup();
  render(h.view);
  await screen.findByText("Please review this");
  await chooseFilter("Mentions");
  h.failSave();
  await user.click(
    screen.getByRole("button", { name: "Open Alice in #Design" }),
  );
  expect(await screen.findByRole("alert")).toHaveTextContent("disk full");
  const detail = screen.getByRole("region", { name: "Inbox detail" });
  await waitFor(() =>
    expect(
      detail.querySelector(`[data-message-id="${h.mention.id}"]`),
    ).toHaveFocus(),
  );
  const retry = screen.getByRole("button", { name: "Retry inbox" });
  const release = h.holdSave();
  h.failSave();
  try {
    retry.focus();
    await user.keyboard("{Enter}");
    await waitFor(() => expect(h.saveStarted()).toBe(true));
    expect(retry).toHaveFocus();
  } finally {
    await act(async () => release());
  }
  await waitFor(() =>
    expect(
      screen.getByRole("list", {
        name: "Inbox conversations",
      }),
    ).toHaveAttribute("aria-busy", "false"),
  );
  expect(retry).toHaveFocus();
  expect(screen.getByRole("alert")).toHaveTextContent("disk full");
  await user.keyboard("{Enter}");
  await waitFor(() =>
    expect(screen.queryByText("disk full")).not.toBeInTheDocument(),
  );
  expect(
    within(detail).getByRole("button", { name: "Close detail" }),
  ).toHaveFocus();
});

it.each([true, false])(
  "source Retry stays mounted through held refresh and hands off list focus only when owned (%s)",
  async (keepFocus) => {
    const h = fixture();
    const user = userEvent.setup();
    render(h.view);
    await screen.findByText("Please review this");
    await waitFor(() =>
      expect(h.owner.session.inboxFeed.snapshot().status).toBe("ready"),
    );
    h.failAux();
    await act(async () => h.owner.session.inboxFeed.refresh());
    expect(screen.getByRole("alert")).toHaveTextContent(
      "auxiliary history unavailable",
    );
    h.failAux(false);
    const release = h.holdAux();
    const retry = screen.getByRole("button", { name: "Retry inbox" });
    const other = screen.getByRole("button", { name: "Unread only" });
    try {
      retry.focus();
      await user.keyboard("{Enter}");
      await waitFor(() =>
        expect(h.owner.session.inboxFeed.snapshot().incomplete).toContain(
          h.mention.id,
        ),
      );
      expect(retry).toHaveFocus();
      expect(retry).toHaveAttribute("aria-disabled", "true");
      if (!keepFocus) other.focus();
    } finally {
      await act(async () => release());
    }
    await waitFor(() =>
      expect(screen.queryByRole("alert")).not.toBeInTheDocument(),
    );
    expect(
      keepFocus ? screen.getByRole("button", { name: "Inbox filters" }) : other,
    ).toHaveFocus();
  },
);

it("combines the existing filter choices without placeholder actions", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  const trigger = screen.getByRole("button", { name: "Inbox filters" });
  expect(trigger).toHaveTextContent("All");
  expect(screen.queryByRole("combobox")).toBeNull();
  expect(screen.queryByRole("tab")).toBeNull();
  expect(
    screen.queryByRole("button", {
      name: /Mark shown as read|Mark as read|Mark unread/,
    }),
  ).toBeNull();
  expect(
    screen.queryByText(
      /Verified recent conversations|Results are bounded|Feed history reached its result limit|Read-state sync is unavailable on this host/,
    ),
  ).toBeNull();
  expect(screen.queryByRole("button", { name: "Refresh" })).toBeNull();
  expect(screen.getByRole("button", { name: "Unread only" })).toHaveAttribute(
    "aria-pressed",
    "false",
  );
  const user = userEvent.setup();
  trigger.focus();
  await user.keyboard("{Enter}");
  const menu = await screen.findByRole("menu", { name: "Inbox filters" });
  expect(
    within(menu).getByRole("menuitemcheckbox", { name: "People" }),
  ).toBeChecked();
  expect(
    within(menu).getByRole("menuitemcheckbox", { name: "Agents" }),
  ).toBeChecked();
  for (const name of ["All", "Threads", "Mentions", "DMs", "Drafts"])
    expect(
      within(menu).getByRole("menuitemradio", { name }),
    ).toBeInTheDocument();
  for (const name of ["Projects", "Needs action", "Reactions"])
    expect(within(menu).queryByRole("menuitemradio", { name })).toBeNull();
  await user.keyboard("{Escape}");
  expect(trigger).toHaveFocus();
});

it("waits for roster recovery before explicit evidence refresh", async () => {
  const h = fixture({ failRoster: true });
  render(h.view);
  expect(await screen.findByRole("alert")).toHaveTextContent("roster offline");
  const baseline = h.evidenceReads();
  const release = h.recoverRoster();
  try {
    fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
    await waitFor(() => expect(h.rosterStarted()).toBe(true));
    expect(h.evidenceReads()).toBe(baseline);
    expect(h.retrySync).toHaveBeenCalledOnce();
  } finally {
    await act(async () => release());
  }
  await screen.findByText("Please review this");
  expect(h.evidenceReads()).toBeGreaterThan(baseline);
});

it("cache clear shows explicit refresh instead of a spinner with no pending work", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await waitFor(() =>
    expect(
      screen.queryByText("Checking recent activity…"),
    ).not.toBeInTheDocument(),
  );
  await act(async () => {
    await h.owner.clearCache();
  });
  expect(screen.getByText("Check recent activity.")).toBeVisible();
  expect(
    screen.queryByText("Checking recent activity…"),
  ).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
  await screen.findByText("Please review this");
  expect(h.retrySync).not.toHaveBeenCalled();
});

it("keeps a failed origin navigation above the retained preview instead of adding a grid column", async () => {
  const h = fixture({ withDm: true, withWriter: true });
  h.open.mockResolvedValue({ status: "failed", reason: "unavailable" });
  render(h.view);
  await screen.findByText("A direct reply");
  await chooseFilter("DMs");
  fireEvent.click(
    screen.getByRole("button", { name: "Open Alice in DM · Alice" }),
  );
  const detail = screen.getByRole("region", { name: "Inbox detail" });
  const preview = within(detail).getByRole("region", {
    name: "Conversation preview",
  });
  fireEvent.click(
    within(detail).getByRole("button", { name: "Open in channel" }),
  );
  const error = await within(detail).findByRole("alert");
  expect(error).toHaveTextContent("This conversation could not be opened");
  // The retained reader has its own hidden/inert wrapper during revalidation;
  // the alert still shares its one detail-body column, above that wrapper.
  const retainedReader = preview.parentElement;
  expect(error.nextElementSibling).toBe(retainedReader);
  expect(error.parentElement).toBe(retainedReader?.parentElement);
  expect(error.parentElement?.parentElement).toBe(detail);
  expect(within(preview).getAllByRole("textbox")).toHaveLength(1);
});

it("does not accept a second row selection while its explicit read is pending", async () => {
  const h = fixture({ withDm: true });
  render(h.view);
  await screen.findByText("A direct reply");
  await waitFor(() =>
    expect(
      screen.queryByText("Checking recent activity…"),
    ).not.toBeInTheDocument(),
  );
  const release = h.holdSave();
  try {
    fireEvent.click(
      screen.getByRole("button", { name: "Open Alice in DM · Alice" }),
    );
    await waitFor(() => expect(h.saveStarted()).toBe(true));
    const other = rows().find((row) =>
      row.textContent?.includes("Please review this"),
    );
    if (!other) throw Error("missing second row");
    const button = within(other).getByRole("button");
    expect(button).toBeDisabled();
    expect(
      screen.getByRole("list", { name: "Inbox conversations" }),
    ).toHaveAttribute("aria-busy", "true");
    fireEvent.click(button);
    expect(
      screen.getByRole("heading", { name: "Direct message" }),
    ).toBeInTheDocument();
    expect(
      within(screen.getByRole("heading", { name: "DM with Alice" })).getByRole(
        "button",
        { name: "DM with Alice" },
      ),
    ).toHaveAttribute("title", "Open conversation");
    expect(h.journal()?.state.frontiers[`msg:${h.mention.id}`]).toBeUndefined();
  } finally {
    await act(async () => release());
  }
  await waitFor(() =>
    expect(
      screen.getByRole("list", { name: "Inbox conversations" }),
    ).toHaveAttribute("aria-busy", "false"),
  );
  const other = rows().find((row) =>
    row.textContent?.includes("Please review this"),
  );
  if (!other) throw Error("missing second row");
  fireEvent.click(within(other).getByRole("button"));
  await waitFor(() =>
    expect(h.journal()?.state.frontiers[`msg:${h.mention.id}`]).toBe(21),
  );
});
it("a failed captured read cannot retry after access retirement", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await chooseFilter("Mentions");
  h.failSave();
  fireEvent.click(
    screen.getByRole("button", { name: "Open Alice in #Design" }),
  );
  expect(await screen.findByRole("alert")).toHaveTextContent("disk full");
  act(() => h.revokeRoom());
  await waitFor(() => expect(rows()).toHaveLength(0));
  fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Inbox action expired",
  );
  expect(h.journal()?.state.frontiers[`msg:${h.mention.id}`]).toBeUndefined();
});
it("closing a pending failed action cancels its retry intent without cancelling admitted storage", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await chooseFilter("Mentions");
  const release = h.holdSave();
  h.failSave();
  try {
    fireEvent.click(
      screen.getByRole("button", { name: "Open Alice in #Design" }),
    );
    await waitFor(() => expect(h.saveStarted()).toBe(true));
    fireEvent.click(screen.getByRole("button", { name: "Close detail" }));
  } finally {
    await act(async () => release());
  }
  expect(await screen.findByRole("alert")).toHaveTextContent("disk full");
  fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
  await waitFor(() =>
    expect(screen.queryByText("disk full")).not.toBeInTheDocument(),
  );
  expect(h.journal()?.state.frontiers[`msg:${h.mention.id}`]).toBeUndefined();
});
it("classifies cached profile-only agents beyond the first 50 without unbounded enrichment", async () => {
  const h = fixture();
  const agent = profile(
    h.profileAgent,
    { name: "Cached outside", is_agent: true },
    100,
  );
  const first = Array.from({ length: 51 }, (_, index) =>
    message(h.unknown, "room", `Unknown ${index}`, 100 + index, [
      ["p", h.viewer.pubkey],
    ]),
  );
  const outside = message(h.profileAgent, "room", "Outside first fifty", 90, [
    ["p", h.viewer.pubkey],
  ]);
  h.events.push(agent, ...first, outside);
  h.emit([agent, ...first, outside]);
  render(h.view);
  await screen.findByText("Unknown 50");
  await waitFor(() =>
    expect(
      screen.queryByText("Checking recent activity…"),
    ).not.toBeInTheDocument(),
  );
  // Roster completion clears optional profiles; restore genuinely shared cache evidence after it.
  act(() => h.emit([agent]));
  await chooseFilter("Agents", "Sender");
  expect(rows()).toHaveLength(1);
  expect(rows()[0]).toHaveTextContent("Outside first fifty");
  expect(
    screen.queryByRole("button", { name: "Show more" }),
  ).not.toBeInTheDocument();
  await chooseFilter("Humans", "Sender");
  expect(
    rows().some((row) => row.textContent?.includes("Outside first fifty")),
  ).toBe(false);
  act(() => h.emit([profile(h.profileAgent, { name: "Cached outside" }, 101)]));
  await waitFor(() =>
    expect(
      rows().some((row) => row.textContent?.includes("Outside first fifty")),
    ).toBe(true),
  );
});

it("feed invalidation has its own local recovery even when shared unread stays ready", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await waitFor(() =>
    expect(h.owner.session.inboxFeed.snapshot().status).toBe("ready"),
  );
  expect(h.owner.session.unread.inbox().status).toBe("ready");
  act(() => h.owner.session.inboxFeed.clear());
  expect(screen.getByRole("button", { name: "Refresh" })).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
  await waitFor(() =>
    expect(h.owner.session.inboxFeed.snapshot().status).toBe("ready"),
  );
  expect(
    screen.queryByRole("button", { name: "Refresh" }),
  ).not.toBeInTheDocument();
});
it("a newer manual intent invalidates retry of an earlier rejected read", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await chooseFilter("Mentions");
  h.failSave();
  fireEvent.click(
    screen.getByRole("button", { name: "Open Alice in #Design" }),
  );
  expect(await screen.findByRole("alert")).toHaveTextContent("disk full");
  await act(async () =>
    h.owner.session.unread.markUnreadLocal({
      kind: "message",
      channelId: "room",
      messageId: h.mention.id,
    }),
  );
  fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Inbox action expired",
  );
  expect(h.journal()?.state.frontiers[`msg:${h.mention.id}`]).toBeUndefined();
  expect(h.journal()?.localUnread[`msg:${h.mention.id}`]).toBeTypeOf("number");
});

it("same-value manual writes after a failed mark-unread supersede retry even without a projection change", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await chooseFilter("Mentions");
  fireEvent.click(
    screen.getByRole("button", { name: "Open Alice in #Design" }),
  );
  await waitFor(() =>
    expect(
      screen.queryByRole("img", { name: "Unread" }),
    ).not.toBeInTheDocument(),
  );
  h.failSave();
  fireEvent.click(await openRowMenu());
  expect(await screen.findByRole("alert")).toHaveTextContent("disk full");
  // An explicit newer read at the same saved frontier has the same visual snapshot.
  await act(async () =>
    h.owner.session.unread.markThrough(
      { kind: "message", channelId: "room", messageId: h.mention.id },
      h.mention.id,
    ),
  );
  fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Inbox action expired",
  );
  expect(h.journal()?.localUnread[`msg:${h.mention.id}`]).toBeUndefined();
});

it("routine delayed publication does not cancel Retry for a failed mark-unread save", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await chooseFilter("Mentions");
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  fireEvent.click(
    screen.getByRole("button", { name: "Open Alice in #Design" }),
  );
  await act(async () => {
    await vi.advanceTimersByTimeAsync(0);
  });
  expect(h.journal()?.state.frontiers[`msg:${h.mention.id}`]).toBe(21);
  const revision = h.owner.session.unread.revision();
  fireEvent.contextMenu(within(rows()[0] as HTMLElement).getByRole("button"), {
    clientX: 20,
    clientY: 20,
  });
  const release = h.holdSave();
  h.failSave();
  try {
    fireEvent.click(screen.getByRole("menuitem", { name: "Mark unread" }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(h.saveStarted()).toBe(true);
  } finally {
    await act(async () => release());
  }
  expect(screen.getByRole("alert")).toHaveTextContent("disk full");
  // Run the already-scheduled publication, including its no-op storage reread.
  await act(async () => {
    await vi.advanceTimersByTimeAsync(5000);
  });
  expect(h.journal()?.acceptedRevision).toBe(revision);
  expect(h.owner.session.unread.revision()).toBe(revision);
  fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
  await act(async () => {
    await vi.advanceTimersByTimeAsync(0);
  });
  expect(h.journal()?.localUnread[`msg:${h.mention.id}`]).toBeTypeOf("number");
  expect(screen.queryByText("disk full")).not.toBeInTheDocument();
});

it.each([true, false])(
  "Inbox activity replies preserve shared recipient policy (session=%s)",
  async (sessionChannel) => {
    const h = fixture({ withWriter: true, sessionChannel, memberAgents: 1 });
    render(h.view);
    await screen.findByText("Please review this");
    await chooseFilter("Mentions");
    fireEvent.click(
      screen.getByRole("button", { name: "Open Alice in #Design" }),
    );
    const editor = await screen.findByRole("textbox", {
      name: "Reply to thread",
    });
    fireEvent.change(editor, { target: { value: "Respond to this" } });
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "Send message" }),
      ).toBeEnabled(),
    );
    fireEvent.click(screen.getByRole("button", { name: "Send message" }));
    await waitFor(() => {
      const alerts = screen.queryAllByRole("alert").map((el) => el.textContent);
      const failed = h.owner.session.outbox
        ?.snapshot()
        .filter((row) => row.delivery === "failed");
      expect({ alerts, failed, published: h.published.length }).toEqual({
        alerts: [],
        failed: [],
        published: 1,
      });
    });
    const sent = h.published[0];
    if (!sent) throw Error("Missing publication");
    expect(sent.tags).toContainEqual(["h", "room"]);
    expect(sent.tags).toContainEqual(["e", h.mention.id, "", "reply"]);
    expect(sent.tags.filter(([name]) => name === "p")).toEqual(
      sessionChannel ? [["p", h.agent.pubkey]] : [],
    );
  },
);
it("Inbox session activity with multiple agents requires an explicit recipient", async () => {
  const h = fixture({
    withWriter: true,
    sessionChannel: true,
    memberAgents: 2,
  });
  render(h.view);
  await screen.findByText("Please review this");
  await chooseFilter("Mentions");
  fireEvent.click(
    screen.getByRole("button", { name: "Open Alice in #Design" }),
  );
  const editor = await screen.findByRole("textbox", {
    name: "Reply to thread",
  });
  fireEvent.change(editor, { target: { value: "Choose someone first" } });
  await waitFor(() =>
    expect(screen.getByRole("button", { name: "Send message" })).toBeEnabled(),
  );
  fireEvent.click(screen.getByRole("button", { name: "Send message" }));
  expect(
    await screen.findByText(
      "There are multiple agents in this session. @mention who should respond.",
    ),
  ).toBeInTheDocument();
  expect(h.published).toEqual([]);
  expect(editor).toHaveValue("Choose someone first");
});

it.each(["preview", "name", "history"])(
  "failed unread Retry survives harmless channel %s replacement",
  async (change) => {
    const h = fixture();
    render(h.view);
    await screen.findByText("Please review this");
    await chooseFilter("Mentions");
    fireEvent.click(
      screen.getByRole("button", { name: "Open Alice in #Design" }),
    );
    await waitFor(() =>
      expect(
        screen.queryByRole("img", { name: "Unread" }),
      ).not.toBeInTheDocument(),
    );
    const before = h.owner.session.channels
      .list()
      .channels.find((channel) => channel.id === "room");
    const revision = h.owner.session.unread.revision();
    const row = h.owner.session.unread
      .inbox()
      .items.find((row) => row.messageId === h.mention.id);
    if (!before || !row) throw Error("Missing ready evidence");
    h.failSave();
    fireEvent.click(await openRowMenu());
    expect(await screen.findByRole("alert")).toHaveTextContent("disk full");
    if (change === "name") act(() => h.renameRoom());
    else if (change === "preview")
      act(() => h.emit([message(h.alice, "room", "Unrelated traffic", 60)]));
    else {
      h.events.push(message(h.alice, "room", "Historical preview", 61));
      act(() => h.owner.session.channels.ensure("room"));
      await waitFor(() =>
        expect(h.owner.session.channels.window("room").status).toBe("ready"),
      );
    }
    await waitFor(() =>
      expect(
        h.owner.session.channels
          .list()
          .channels.find((channel) => channel.id === "room"),
      ).not.toBe(before),
    );
    expect(h.owner.session.unread.revision()).toBe(revision);
    expect(
      h.owner.session.unread.inbox().items.find((item) => item.id === row.id),
    ).toEqual(row);
    fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
    await waitFor(() =>
      expect(h.journal()?.localUnread[`msg:${h.mention.id}`]).toBeTypeOf(
        "number",
      ),
    );
  },
);
it("revoke and regrant cannot revive a captured failed Inbox mutation", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await chooseFilter("Mentions");
  h.failSave();
  fireEvent.click(
    screen.getByRole("button", { name: "Open Alice in #Design" }),
  );
  expect(await screen.findByRole("alert")).toHaveTextContent("disk full");
  act(() => {
    h.revokeRoom();
    h.restoreRoom();
  });
  await waitFor(() => expect(rows()).toHaveLength(1));
  fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
  expect(
    await screen.findByText(
      "Inbox action expired. Close and reopen the conversation.",
    ),
  ).toBeInTheDocument();
  expect(h.journal()?.state.frontiers[`msg:${h.mention.id}`]).toBeUndefined();
});

it.each(["read-only", "unsupported"] as const)(
  "%s host cannot create an unread mark it cannot clear",
  async (readCapability) => {
    const h = fixture({ readCapability });
    render(h.view);
    await screen.findByText("Please review this");
    await chooseFilter("Mentions");
    await waitFor(() =>
      expect(
        screen.queryByText("Checking recent activity…"),
      ).not.toBeInTheDocument(),
    );
    expect(h.owner.session.unread.sync().capability).toBe(readCapability);
    expect(rows()).toHaveLength(1);
    expect(
      within(rows()[0] as HTMLElement).queryByRole("img", { name: "Unread" }),
    ).not.toBeInTheDocument();
    const before = h.journal();
    if (!before) throw Error("Missing saved read state");
    fireEvent.click(
      screen.getByRole("button", { name: "Open Alice in #Design" }),
    );
    await screen.findByRole("region", { name: "Thread messages" });
    const action = await openRowMenu();
    expect(action).toHaveAttribute("aria-disabled", "true");
    fireEvent.click(action);
    await act(async () => {});
    expect(h.journal()).toEqual(before);
    expect(h.journal()?.localUnread).toEqual({ [`msg:${"f".repeat(64)}`]: 1 });
  },
);
it("retiring a session removes its open unread menu without another write", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await chooseFilter("Mentions");
  fireEvent.click(
    screen.getByRole("button", { name: "Open Alice in #Design" }),
  );
  await waitFor(() =>
    expect(
      screen.queryByRole("img", { name: "Unread" }),
    ).not.toBeInTheDocument(),
  );
  const action = await openRowMenu();
  const before = h.journal();
  act(() => h.disconnect());
  expect(action).not.toBeInTheDocument();
  await act(async () => {});
  expect(h.journal()).toEqual(before);
});

it("a two-step read retries its rejected thread prefix without losing the saved first step", async () => {
  const h = fixture();
  const root = message(h.alice, "room", "Partial root", 30, [
    ["p", h.viewer.pubkey],
  ]);
  const child = message(h.alice, "room", "Partial reply", 31, [
    ["p", h.viewer.pubkey],
    ["e", root.id, "", "reply"],
  ]);
  h.events.push(root, child);
  h.emit([root, child]);
  render(h.view);
  await screen.findByText("Partial root");
  await waitFor(() =>
    expect(
      screen.queryByText("Checking recent activity…"),
    ).not.toBeInTheDocument(),
  );
  h.failThreadSave();
  const row = rows().find((row) => row.textContent?.includes("Partial root"));
  if (!row) throw Error("Missing two-step row");
  fireEvent.click(within(row).getByRole("button"));
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "thread disk full",
  );
  expect(h.readSteps.map((step) => step.id)).toEqual([root.id, child.id]);
  expect(h.journal()?.state.frontiers[`msg:${root.id}`]).toBe(30);
  expect(h.journal()?.state.frontiers[`thread:${root.id}`]).toBeUndefined();
  fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
  await waitFor(() =>
    expect(h.journal()?.state.frontiers[`thread:${root.id}`]).toBe(31),
  );
  expect(h.journal()?.state.frontiers[`msg:${root.id}`]).toBe(30);
});

it("All activity stays chat-only when addressed project and approval evidence arrives", async () => {
  const h = fixture();
  const nonchat = [1621, 46010].map((kind) =>
    signed(h.alice, {
      kind,
      content: `Not an Inbox conversation ${kind}`,
      created_at: 40,
      tags: [
        ["h", "room"],
        ["p", h.viewer.pubkey],
        ["a", `30617:${h.alice.pubkey}:repo`],
      ],
    }),
  );
  h.events.push(...nonchat);
  render(h.view);
  await screen.findByText("Please review this");
  await waitFor(() =>
    expect(h.owner.session.inboxFeed.snapshot().status).toBe("ready"),
  );
  expect(rows()).toHaveLength(2);
  act(() => h.emit(nonchat));
  expect(rows()).toHaveLength(2);
  expect(screen.queryByText(/Not an Inbox conversation/)).toBeNull();
  expect(screen.queryByRole("button", { name: "Open in project" })).toBeNull();
});

it("keeps the captured reply selected when its older root is verified", async () => {
  const h = fixture();
  const unknownRoot = message(h.alice, "room", "Late root", 39);
  const reply = message(h.alice, "room", "Orphan addressed reply", 42, [
    ["p", h.viewer.pubkey],
    ["e", unknownRoot.id, "", "reply"],
  ]);
  h.events.push(reply);
  h.emit([reply]);
  render(h.view);
  await screen.findByText("Orphan addressed reply");
  const row = rows().find((item) =>
    item.textContent?.includes("Orphan addressed reply"),
  );
  if (!row) throw Error("Missing orphan row");
  const open = within(row).getByRole("button", { name: /^Open / });
  fireEvent.click(open);
  expect(
    screen.getByRole("region", { name: "Inbox detail" }),
  ).toBeInTheDocument();
  expect(open).toHaveAttribute("aria-current", "page");
  act(() => h.emit([unknownRoot]));
  expect(
    screen.getByRole("region", { name: "Inbox detail" }),
  ).toBeInTheDocument();
  const regrouped = rows().find((item) =>
    item.textContent?.includes("Orphan addressed reply"),
  );
  if (!regrouped) throw Error("Missing regrouped row");
  expect(
    within(regrouped).getByRole("button", { name: /^Open / }),
  ).toHaveAttribute("aria-current", "page");
  fireEvent.click(screen.getByRole("button", { name: "Open in channel" }));
  await waitFor(() =>
    expect(h.open).toHaveBeenCalledWith({
      version: 1,
      kind: "conversation",
      scope: h.scope,
      channelId: "room",
      messageId: reply.id,
    }),
  );
});

it("shows only the exact unfinished row's placeholder through held edits and retry, never an unverified body", async () => {
  const h = fixture();
  const old = message(h.alice, "room", "OLD BODY", 39, [
    ["p", h.viewer.pubkey],
  ]);
  const edit = signed(h.alice, {
    kind: 40003,
    content: "CURRENT BODY",
    created_at: 40,
    tags: [
      ["h", "room"],
      ["e", old.id],
    ],
  });
  h.events.push(old, edit);
  const release = h.holdAux();
  render(h.view);
  try {
    await waitFor(() =>
      expect(h.owner.session.inboxFeed.snapshot().incomplete).toContain(old.id),
    );
    const unfinished = rows().find((row) =>
      row.textContent?.includes("Preview updating…"),
    );
    if (!unfinished) throw Error("Missing pending preview row");
    expect(unfinished).not.toHaveTextContent("OLD BODY");
    expect(
      within(unfinished).getByRole("button", { name: /^Open / }),
    ).toHaveAccessibleDescription("Preview updating…");
    const unaffected = rows().find((row) =>
      row.textContent?.includes("A thread update"),
    );
    expect(unaffected).toBeDefined();
    expect(unaffected).not.toHaveTextContent("Preview updating…");
    fireEvent.click(within(unfinished).getByRole("button", { name: /^Open / }));
    expect(
      screen.getByRole("region", { name: "Inbox detail" }),
    ).toHaveTextContent("Preview updating…");
    expect(
      screen.getByRole("region", { name: "Inbox detail" }),
    ).not.toHaveTextContent("OLD BODY");
    h.failAux();
  } finally {
    await act(async () => release());
  }
  await waitFor(() =>
    expect(
      screen.getByRole("region", { name: "Inbox detail" }),
    ).toHaveTextContent("Preview unavailable. Retry inbox."),
  );
  expect(
    screen.getByRole("button", {
      name: /^Open /,
      current: "page",
    }),
  ).toHaveAccessibleDescription("Preview unavailable. Retry inbox.");
  h.failAux(false);
  fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
  await waitFor(() =>
    expect(h.owner.session.inboxFeed.snapshot()).toMatchObject({
      status: "ready",
      incomplete: [],
    }),
  );
  expect(
    screen.getByRole("button", {
      name: /^Open /,
      current: "page",
    }),
  ).toHaveAccessibleDescription("CURRENT BODY");
  expect(rows().some((row) => row.textContent?.includes("CURRENT BODY"))).toBe(
    true,
  );
  expect(rows().some((row) => row.textContent?.includes("OLD BODY"))).toBe(
    false,
  );
});

it("freezes the top-level origin coordinate when read-state switches representative to a reply", async () => {
  const h = fixture();
  const root = message(h.alice, "room", "New root mention", 30, [
    ["p", h.viewer.pubkey],
  ]);
  const reply = message(h.alice, "room", "New reply mention", 31, [
    ["p", h.viewer.pubkey],
    ["e", root.id, "", "reply"],
  ]);
  h.addEvent(root);
  h.addEvent(reply);
  h.emit([root, reply]);
  render(h.view);
  await screen.findByText("New root mention");
  const row = rows().find((item) =>
    item.textContent?.includes("New root mention"),
  );
  if (!row) throw Error("Missing root mention");
  fireEvent.click(within(row).getByRole("button", { name: /^Open / }));
  await waitFor(() =>
    expect(h.journal()?.state.frontiers[`thread:${root.id}`]).toBe(31),
  );
  expect(
    screen.getByRole("region", { name: "Inbox detail" }),
  ).toBeInTheDocument();
  // Reading changes the row representative, not the already captured visit.
  const selectedRow = rows().find((item) =>
    item.textContent?.includes("New reply mention"),
  );
  if (!selectedRow) throw Error("Missing regrouped selected row");
  fireEvent.click(within(selectedRow).getByRole("button", { name: /^Open / }));
  fireEvent.click(screen.getByRole("button", { name: "Open in channel" }));
  await waitFor(() =>
    expect(h.open).toHaveBeenCalledWith({
      version: 1,
      kind: "conversation",
      scope: h.scope,
      channelId: "room",
      messageId: root.id,
    }),
  );
});

it("retires a deleted selected row and restores focus to a still-visible fallback", async () => {
  const h = fixture({ withDm: true });
  render(h.view);
  await screen.findByText("A direct reply");
  const mention = rows().find((row) =>
    row.textContent?.includes("Please review this"),
  );
  if (!mention) throw Error("Missing mention row");
  fireEvent.click(within(mention).getByRole("button", { name: /^Open / }));
  await screen.findByRole("region", { name: "Inbox detail" });
  act(() =>
    h.emit([
      signed(h.alice, {
        kind: 5,
        content: "",
        created_at: 99,
        tags: [
          ["h", "room"],
          ["e", h.mention.id],
        ],
      }),
    ]),
  );
  await waitFor(() =>
    expect(
      screen.queryByRole("region", { name: "Inbox detail" }),
    ).not.toBeInTheDocument(),
  );
  const fallback = rows()[0];
  if (!fallback) throw Error("Missing fallback row");
  await waitFor(() =>
    expect(
      within(fallback).getByRole("button", { name: /^Open / }),
    ).toHaveFocus(),
  );
  expect(
    rows().some((row) => row.textContent?.includes("Please review this")),
  ).toBe(false);
});

it("waits for a held read before restoring focus after a selected row is deleted", async () => {
  const h = fixture({ withDm: true });
  render(h.view);
  await screen.findByText("A direct reply");
  const mention = rows().find((row) =>
    row.textContent?.includes("Please review this"),
  );
  if (!mention) throw Error("Missing mention row");
  const release = h.holdSave();
  try {
    fireEvent.click(within(mention).getByRole("button", { name: /^Open / }));
    await waitFor(() => expect(h.saveStarted()).toBe(true));
    act(() =>
      h.emit([
        signed(h.alice, {
          kind: 5,
          content: "",
          created_at: 99,
          tags: [
            ["h", "room"],
            ["e", h.mention.id],
          ],
        }),
      ]),
    );
    await waitFor(() =>
      expect(
        screen.queryByRole("region", { name: "Inbox detail" }),
      ).not.toBeInTheDocument(),
    );
    const fallback = rows()[0];
    if (!fallback) throw Error("Missing fallback row");
    expect(
      within(fallback).getByRole("button", { name: /^Open / }),
    ).toBeDisabled();
  } finally {
    await act(async () => release());
  }
  const fallback = rows()[0];
  if (!fallback) throw Error("Missing fallback row after save");
  await waitFor(() =>
    expect(
      within(fallback).getByRole("button", { name: /^Open / }),
    ).toHaveFocus(),
  );
});

it("keeps the exact selected group visibly incomplete when a held reply regroups under an older root", async () => {
  const h = fixture();
  const root = message(h.alice, "room", "Older root", 29);
  const reply = message(h.alice, "room", "ORIGINAL REPLY", 30, [
    ["p", h.viewer.pubkey],
    ["e", root.id, "", "reply"],
  ]);
  const edit = signed(h.alice, {
    kind: 40003,
    content: "Edited reply",
    created_at: 31,
    tags: [
      ["h", "room"],
      ["e", reply.id],
    ],
  });
  h.addEvent(reply);
  h.addEvent(edit);
  const release = h.holdAux();
  render(h.view);
  try {
    await waitFor(() =>
      expect(h.owner.session.inboxFeed.snapshot().incomplete).toContain(
        reply.id,
      ),
    );
    const original = rows().find((row) =>
      row.textContent?.includes("Preview updating…"),
    );
    if (!original) throw Error("No pending orphan reply");
    fireEvent.click(within(original).getByRole("button", { name: /^Open / }));
    act(() => h.emit([root]));
    expect(
      h.owner.session.unread
        .inbox()
        .items.some(
          (row) =>
            row.id === `room:${root.id}` && row.messageIds.includes(reply.id),
        ),
    ).toBe(true);
    expect(
      screen.getByRole("region", { name: "Inbox detail" }),
    ).toHaveTextContent("Preview updating…");
    expect(
      screen.getByRole("region", { name: "Inbox detail" }),
    ).not.toHaveTextContent("ORIGINAL REPLY");
  } finally {
    await act(async () => release());
  }
  await waitFor(() =>
    expect(h.owner.session.inboxFeed.snapshot().incomplete).toEqual([]),
  );
  expect(rows().some((row) => row.textContent?.includes("Edited reply"))).toBe(
    true,
  );
});

it("retries the remaining captured prefix after a harmless new reply while the second save fails", async () => {
  const h = fixture();
  const root = message(h.alice, "room", "Retry root", 30, [
    ["p", h.viewer.pubkey],
  ]);
  const child = message(h.alice, "room", "Retry reply", 31, [
    ["p", h.viewer.pubkey],
    ["e", root.id, "", "reply"],
  ]);
  const arriving = message(h.alice, "room", "Later reply", 32, [
    ["p", h.viewer.pubkey],
    ["e", root.id, "", "reply"],
  ]);
  h.addEvent(root);
  h.addEvent(child);
  h.emit([root, child]);
  render(h.view);
  await screen.findByText("Retry root");
  await waitFor(() =>
    expect(
      screen.queryByText("Checking recent activity…"),
    ).not.toBeInTheDocument(),
  );
  h.failThreadSave();
  const row = rows().find((item) => item.textContent?.includes("Retry root"));
  if (!row) throw Error("Missing retry row");
  fireEvent.click(within(row).getByRole("button", { name: /^Open / }));
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "thread disk full",
  );
  expect(h.journal()?.state.frontiers[`msg:${root.id}`]).toBe(30);
  expect(h.journal()?.state.frontiers[`thread:${root.id}`]).toBeUndefined();
  h.addEvent(arriving);
  act(() => h.emit([arriving]));
  fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
  await waitFor(() =>
    expect(h.journal()?.state.frontiers[`thread:${root.id}`]).toBe(31),
  );
  expect(h.readSteps.map((step) => step.id)).toEqual([
    root.id,
    child.id,
    child.id,
  ]);
  expect(h.journal()?.state.frontiers[`msg:${root.id}`]).toBe(30);
  expect(
    h.owner.session.unread
      .inbox()
      .items.find((item) => item.id === `room:${root.id}`)?.messageIds,
  ).toContain(arriving.id);
});

it("preserves a failed captured read across verified root regrouping", async () => {
  const h = fixture();
  const root = message(h.alice, "room", "Late regroup root", 29);
  const orphan = message(h.alice, "room", "Regrouped reply", 30, [
    ["p", h.viewer.pubkey],
    ["e", root.id, "", "reply"],
  ]);
  h.addEvent(orphan);
  h.emit([orphan]);
  render(h.view);
  await screen.findByText("Regrouped reply");
  h.failSave();
  const row = rows().find((candidate) =>
    candidate.textContent?.includes("Regrouped reply"),
  );
  if (!row) throw Error("Missing orphan");
  fireEvent.click(within(row).getByRole("button", { name: /^Open / }));
  expect(await screen.findByRole("alert")).toHaveTextContent("disk full");
  h.addEvent(root);
  act(() => h.emit([root]));
  expect(
    h.owner.session.unread
      .inbox()
      .items.some(
        (item) =>
          item.id === `room:${root.id}` && item.messageIds.includes(orphan.id),
      ),
  ).toBe(true);
  fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
  await waitFor(() =>
    expect(h.journal()?.state.frontiers[`msg:${orphan.id}`]).toBe(30),
  );
  expect(h.readSteps.map((step) => step.id)).toEqual([orphan.id, orphan.id]);
});

it("DM read Retry retains the original cutoff and survives re-click of the selected row", async () => {
  const clickedAt = Math.floor(Date.now() / 1000);
  const now = vi.spyOn(Date, "now").mockReturnValue(clickedAt * 1000);
  const h = fixture({ withDm: true });
  const releaseHistory = h.holdHistory("dm-room");
  try {
    render(h.view);
    await screen.findByText("A direct reply");
    await waitFor(() =>
      expect(
        screen.queryByText("Checking recent activity…"),
      ).not.toBeInTheDocument(),
    );
    const dmRow = rows().find((item) =>
      item.textContent?.includes("A direct reply"),
    );
    if (!dmRow) throw Error("Missing DM row");
    const row = within(dmRow).getByRole("button", { name: /^Open / });
    h.failSave();
    fireEvent.click(row);
    expect(await screen.findByRole("alert")).toHaveTextContent("disk full");
    fireEvent.click(row);
    now.mockReturnValue((clickedAt + 120) * 1000);
    const later = message(
      h.alice,
      "dm-room",
      "Arrived after failed read",
      clickedAt + 61,
    );
    h.addEvent(later);
    act(() => h.emit([later]));
    fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
    await waitFor(() => {
      const journal = h.journal();
      if (!journal) throw Error("Expected saved read journal");
      expect(journal.state.frontiers["dm-room"]).toBe(clickedAt);
    });
    expect(h.owner.session.unread.attention("dm-room", later.id).unread).toBe(
      true,
    );
  } finally {
    await act(async () => releaseHistory());
    now.mockRestore();
  }
});

it("the Inbox owner dismisses an in-head DM with Escape but respects consumed child Escape", async () => {
  const h = fixture({ withDm: true });
  render(h.view);
  await screen.findByText("A direct reply");
  const row = screen.getByRole("button", { name: "Open Alice in DM · Alice" });
  fireEvent.click(row);
  const detail = await screen.findByRole("region", { name: "Inbox detail" });
  const close = await within(detail).findByRole("button", {
    name: "Close detail",
  });
  const consume = (event: KeyboardEvent) => event.preventDefault();
  close.addEventListener("keydown", consume);
  fireEvent.keyDown(close, { key: "Escape" });
  expect(detail).toBeInTheDocument();
  close.removeEventListener("keydown", consume);
  fireEvent.keyDown(close, { key: "Escape" });
  await waitFor(() => expect(detail).not.toBeInTheDocument());
  await waitFor(() => expect(row).toHaveFocus());
});

it("Escape dismisses an incomplete detail without waiting for auxiliary history", async () => {
  const h = fixture();
  const release = h.holdAux();
  try {
    render(h.view);
    await screen.findByText("Preview updating…");
    await chooseFilter("Mentions");
    const user = userEvent.setup();
    screen.getByRole("button", { name: "Open Alice in #Design" }).focus();
    await user.keyboard("{Enter}");
    const detail = await screen.findByRole("region", { name: "Inbox detail" });
    expect(within(detail).getByRole("status")).toHaveTextContent(
      "Preview updating…",
    );
    expect(
      within(detail).getByRole("button", { name: "Close detail" }),
    ).toHaveFocus();
    // Status/profile updates in this same placeholder visit must not steal focus.
    const other = within(detail).getByRole("button", {
      name: "Open in channel",
    });
    other.focus();
    h.failAux();
    await act(async () => release());
    await waitFor(() =>
      expect(detail).toHaveTextContent("Preview unavailable. Retry inbox."),
    );
    expect(other).toHaveFocus();
    await user.keyboard("{Escape}");
    expect(detail).not.toBeInTheDocument();
  } finally {
    await act(async () => release());
  }
});

it.each(["close", "delete", "delete-pending"])(
  "%s of the last filtered conversation restores a stable visible control",
  async (action) => {
    const h = fixture();
    render(h.view);
    await screen.findByText("Please review this");
    await chooseFilter("Mentions");
    fireEvent.click(screen.getByRole("button", { name: "Unread only" }));
    expect(rows()).toHaveLength(1);
    const release = action === "delete-pending" ? h.holdSave() : () => {};
    try {
      fireEvent.click(
        screen.getByRole("button", { name: "Open Alice in #Design" }),
      );
      await screen.findByRole("region", { name: "Inbox detail" });
      if (action === "delete-pending")
        await waitFor(() => expect(h.saveStarted()).toBe(true));
      else
        await waitFor(() =>
          expect(
            screen.queryByRole("img", { name: "Unread" }),
          ).not.toBeInTheDocument(),
        );
      if (action === "close")
        fireEvent.click(screen.getByRole("button", { name: "Close detail" }));
      else
        act(() =>
          h.emit([
            signed(h.alice, {
              kind: 5,
              content: "",
              created_at: 99,
              tags: [
                ["h", "room"],
                ["e", h.mention.id],
              ],
            }),
          ]),
        );
      await waitFor(() =>
        expect(
          screen.queryByRole("region", { name: "Inbox detail" }),
        ).not.toBeInTheDocument(),
      );
    } finally {
      await act(async () => release());
    }
    await waitFor(() => expect(rows()).toHaveLength(0));
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "Inbox filters" }),
      ).toHaveFocus(),
    );
  },
);

it("Retry with a ready roster waits for both evidence reads before retrying sync", async () => {
  const h = fixture();
  render(h.view);
  await screen.findByText("Please review this");
  await waitFor(() =>
    expect(h.owner.session.inboxFeed.snapshot().status).toBe("ready"),
  );
  h.failAux();
  await act(async () => h.owner.session.inboxFeed.refresh());
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "auxiliary history unavailable",
  );
  h.failAux(false);
  const release = h.holdAux();
  try {
    fireEvent.click(screen.getByRole("button", { name: "Retry inbox" }));
    await waitFor(() =>
      expect(h.owner.session.inboxFeed.snapshot().incomplete).toContain(
        h.mention.id,
      ),
    );
    expect(h.retrySync).not.toHaveBeenCalled();
    expect(
      screen.getByRole("list", { name: "Inbox conversations" }),
    ).toHaveAttribute("aria-busy", "true");
  } finally {
    await act(async () => release());
  }
  await waitFor(() => expect(h.retrySync).toHaveBeenCalledOnce());
  await waitFor(() =>
    expect(
      screen.getByRole("list", { name: "Inbox conversations" }),
    ).toHaveAttribute("aria-busy", "false"),
  );
});

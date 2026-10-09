// @vitest-environment jsdom
import "@testing-library/jest-dom/vitest";
import { StrictMode, useState, useSyncExternalStore } from "react";
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
import { InboxDetail } from "./InboxDetail";
import type { ComposerInputElement } from "../../features/messages/composer-dom";
import { composerDOMFixture } from "../../features/messages/composer-testing";
import { createRelaySession } from "../../features/relay/session";
import type { LiveCallbacks } from "../../features/relay/live";
import type { Navigation } from "../../features/navigation/controller";
import { matchesEvent } from "../../features/relay/projection";
import {
  keypair,
  message,
  metadata,
  roster,
  signed,
} from "../../features/relay/testing";

import {
  readJournal,
  type ReadJournal,
} from "../../features/relay/read-state-storage";
import {
  decodeReadState,
  signReadState,
  // @ts-expect-error Node host codec, with disposable test identities only.
} from "../../../browser-host/read-state.mjs";

composerDOMFixture();
const scrollDescriptor = Object.getOwnPropertyDescriptor(
  HTMLElement.prototype,
  "scrollIntoView",
);
const navigator: Navigation = {
  snapshot() {
    throw new Error("Navigation snapshot unused in detail");
  },
  subscribe: () => () => {},
  open: async () => ({ status: "opened" }),
  retry: async () => ({ status: "opened" }),
  back() {},
  forward() {},
};
const owners: ReturnType<typeof createRelaySession>[] = [];
let frames = new Map<number, FrameRequestCallback>();
let nextFrame = 0;
beforeEach(() => {
  localStorage.clear();
  vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue();
  vi.spyOn(HTMLMediaElement.prototype, "pause").mockImplementation(() => {});
  frames = new Map();
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    frames.set(++nextFrame, callback);
    return nextFrame;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => frames.delete(id));
  // Synthetic layout lets the actual reveal hook complete and observe dwell.
  // Browser paint, scrolling and native inert behavior remain browser contracts.
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue(
    new DOMRect(0, 0, 100, 20),
  );
  vi.spyOn(HTMLElement.prototype, "getClientRects").mockReturnValue([
    new DOMRect(0, 0, 100, 20),
  ] as unknown as DOMRectList);
  vi.spyOn(document, "hasFocus").mockReturnValue(true);
  Object.defineProperty(HTMLElement.prototype, "scrollIntoView", {
    configurable: true,
    value: vi.fn(),
  });
});
afterEach(() => {
  cleanup();
  for (const owner of owners.splice(0)) owner.dispose();
  if (scrollDescriptor)
    Object.defineProperty(
      HTMLElement.prototype,
      "scrollIntoView",
      scrollDescriptor,
    );
  else Reflect.deleteProperty(HTMLElement.prototype, "scrollIntoView");
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});
function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
async function frame() {
  await act(async () => {
    const pending = [...frames.values()];
    frames.clear();
    for (const callback of pending) callback(performance.now());
  });
}
async function fixture() {
  const viewer = keypair(),
    alice = keypair(),
    relay = keypair();
  const root = message(
    alice,
    "room",
    "Selected addressed body\n\nhttps://fixture.test/video.mp4",
    20,
    [
      ["p", viewer.pubkey],
      ["imeta", "url https://fixture.test/video.mp4", "m video/mp4"],
      ["imeta", "url https://fixture.test/image.png", "m image/png"],
    ],
  );
  const events = [
    roster(relay, "room", [viewer.pubkey, alice.pubkey], 10),
    metadata(relay, "room", "Room", 10),
    root,
  ];
  let journal: ReadJournal | undefined;
  let live!: LiveCallbacks;
  let auxiliary:
    | {
        gate: ReturnType<typeof deferred>;
        started: ReturnType<typeof deferred>;
        fail: boolean;
      }
    | undefined;
  const owner = createRelaySession(
    {
      viewer: viewer.pubkey,
      relayAuthor: relay.pubkey,
      query: async (filters) => {
        if (
          filters.some(
            (filter) => filter["#e"] && filter.kinds?.includes(40003),
          ) &&
          auxiliary
        ) {
          const held = auxiliary;
          held.started.resolve();
          await held.gate.promise;
          if (held.fail) throw new Error("auxiliary closure unavailable");
        }
        return filters.flatMap((filter) =>
          events
            .filter((event) => matchesEvent(event, filter))
            .slice(0, filter.limit),
        );
      },
      media: (url) => url,
      readState: {
        decode: async (records) => decodeReadState(records, viewer.secret),
        sign: async (intent) => signReadState(intent, viewer.secret),
        publish: async (event) => {
          events.push(event);
        },
      },
      writer: {
        kinds: [9],
        sign: async (template) => signed(viewer, template),
        publish: async () => {},
      },
      subscribe(callbacks) {
        live = callbacks;
        return { update() {}, retry() {}, dispose() {} };
      },
    },
    {
      outboxStorage: { load: () => [], save: () => {} },
      readStateStorage: {
        async update(change) {
          journal = readJournal(change(journal), viewer.pubkey);
          return journal;
        },
        close() {},
      },
      readPublisherLock: async (_signal, work) => work(),
    },
  );
  owners.push(owner);
  live.state({ status: "connected", routes: [] });
  live.receive(events);
  owner.session.channels.ensureList();
  await waitFor(() =>
    expect(owner.session.channels.list().status).toBe("ready"),
  );
  const item = owner.session.unread.inbox().items[0];
  if (!item) throw new Error("Missing addressed conversation");
  return {
    ...owner,
    live,
    root,
    item,
    journal: () => journal,
    scope: { viewer: viewer.pubkey, communityOrigin: "https://relay.test" },
    hold(fail = false) {
      auxiliary = { gate: deferred(), started: deferred(), fail };
      return auxiliary;
    },
    revoke() {
      const removed = roster(relay, "room", [alice.pubkey], 100);
      events[0] = removed;
      live.receive([removed]);
    },
  };
}
function Visit({ h }: { h: Awaited<ReturnType<typeof fixture>> }) {
  const [visit, setVisit] = useState(1);
  const feed = useSyncExternalStore(
    h.session.inboxFeed.subscribe,
    h.session.inboxFeed.snapshot,
    h.session.inboxFeed.snapshot,
  );
  return (
    <>
      <label>
        Sender
        <select defaultValue="all">
          <option value="all">All senders</option>
        </select>
      </label>
      <button type="button" onClick={() => setVisit((value) => value + 1)}>
        Retarget
      </button>
      {visit > 0 && (
        <InboxDetail
          key={visit}
          item={h.item}
          target={{ channelId: "room", messageId: h.root.id }}
          session={h.session}
          scope={h.scope}
          navigator={navigator}
          channelName="Room"
          onBack={() => setVisit(0)}
          previewIncomplete={
            feed.incomplete.includes(h.root.id)
              ? feed.status === "error"
                ? "error"
                : "loading"
              : undefined
          }
        />
      )}
    </>
  );
}
async function opened(h: Awaited<ReturnType<typeof fixture>>) {
  await h.session.inboxFeed.ensure();
  render(
    <StrictMode>
      <Visit h={h} />
    </StrictMode>,
  );
  const reader = await screen.findByRole("complementary", { name: "Thread" });
  const editor = (await screen.findByRole("textbox")) as ComposerInputElement;
  await waitFor(() =>
    expect(
      reader.querySelector(`[data-message-id="${h.root.id}"]`),
    ).not.toBeNull(),
  );
  // Reveal installs its observer, focuses, then verifies visibility on three frames.
  await frame();
  await frame();
  await frame();
  const row = reader.querySelector(`[data-message-id="${h.root.id}"]`);
  expect(row).toHaveFocus();
  return { reader, editor, row };
}
it("opens the captured message from the channel heading", async () => {
  const h = await fixture();
  await opened(h);
  const open = vi.spyOn(navigator, "open");
  const heading = screen.getByRole("heading", { name: "#Room" });
  const link = within(heading).getByRole("button", { name: "#Room" });
  expect(link).toHaveAttribute("title", "Open in channel");
  fireEvent.click(link);
  await waitFor(() =>
    expect(open).toHaveBeenCalledWith({
      version: 1,
      kind: "conversation",
      scope: h.scope,
      channelId: "room",
      messageId: h.root.id,
    }),
  );
});

it.each([false, true])(
  "retains the selected reader through reconnect closure (failure: %s), without replaying reveal",
  async (fail) => {
    const h = await fixture();
    const { reader, editor } = await opened(h);
    const reveals = vi.mocked(HTMLElement.prototype.scrollIntoView).mock.calls
      .length;
    fireEvent.click(
      screen.getByRole("button", { name: "Open video fullscreen" }),
    );
    expect(
      screen.getByRole("dialog", { name: "Video attachment" }),
    ).toBeVisible();
    const sender = screen.getByRole("combobox", {
      name: "Sender",
      hidden: true,
    });
    // Unlike jsdom, browsers reject focus in the app while the retiring modal
    // still owns its body siblings' inert state. Its passive cleanup releases it.
    const focus = HTMLElement.prototype.focus;
    vi.spyOn(HTMLElement.prototype, "focus").mockImplementation(function (
      this: HTMLElement,
      options,
    ) {
      let ancestor: HTMLElement | null = this;
      while (ancestor) {
        if (ancestor.inert || ancestor.hidden || ancestor.hasAttribute("inert"))
          return;
        ancestor = ancestor.parentElement;
      }
      focus.call(this, options);
    });
    const gate = h.hold(fail);
    try {
      act(() => {
        h.live.state({ status: "retrying", routes: [] });
        h.live.state({ status: "connected", routes: [] });
        h.live.established();
      });
      await act(async () => {
        await gate.started.promise;
      });
      expect(screen.getByText("Preview updating…")).toBeVisible();
      expect(document.body.querySelector('[role="dialog"]')).toBeNull();
      expect(
        screen.getByRole("button", { name: "Close detail" }),
      ).toHaveFocus();
      act(() => sender.focus());
      expect(reader).toBeInTheDocument();
      expect(editor).toBeInTheDocument();
      expect(reader).not.toBeVisible();
      expect(reader.closest("[inert][hidden]")).not.toBeNull();
      expect(
        screen.queryByRole("complementary", { name: "Thread" }),
      ).toBeNull();
      expect(screen.queryByRole("textbox")).toBeNull();
      expect(sender).toHaveFocus();
      await act(async () => {
        gate.gate.resolve();
      });
      if (fail) {
        await screen.findByText("Preview unavailable. Retry inbox.");
        expect(reader).not.toBeVisible();
        expect(editor).toBeInTheDocument();
        await act(async () => {
          await h.session.inboxFeed.refresh();
        });
        // A failed retry must not expose the stale reader either.
        expect(
          screen.getByText("Preview unavailable. Retry inbox."),
        ).toBeVisible();
        gate.fail = false;
        await act(async () => {
          await h.session.inboxFeed.refresh();
        });
      }
      await waitFor(() =>
        expect(h.session.inboxFeed.snapshot().status).toBe("ready"),
      );
      expect(screen.getByRole("complementary", { name: "Thread" })).toBe(
        reader,
      );
      expect(screen.getByRole("textbox")).toBe(editor);
      expect(reader).toBeVisible();
      expect(document.body.querySelector('[role="dialog"]')).toBeNull();
      await frame();
      await frame();
      await frame();
      expect(sender).toHaveFocus();
      expect(HTMLElement.prototype.scrollIntoView).toHaveBeenCalledTimes(
        reveals,
      );
    } finally {
      gate.gate.resolve();
    }
  },
);
it("Escape dismisses a thread's fullscreen image without closing the detail", async () => {
  const h = await fixture();
  const { reader, editor } = await opened(h);
  const thumbnail = within(reader).getByRole("link", {
    name: "Open image attachment",
  });
  fireEvent.click(thumbnail, { detail: 1 });
  const close = within(
    screen.getByRole("dialog", { name: "Image attachment" }),
  ).getByRole("button", { name: "Close fullscreen viewer" });
  expect(close).toHaveFocus();
  // The portal bubbles through ThreadPanel and InboxDetail in React.
  fireEvent.keyDown(close, { key: "Escape" });
  expect(document.body.querySelector('[role="dialog"]')).toBeNull();
  expect(screen.getByRole("complementary", { name: "Thread" })).toBe(reader);
  expect(screen.getByRole("textbox")).toBe(editor);
  await frame();
  expect(thumbnail).toHaveFocus();
});
it("withholds first-incomplete admission and retires an admitted reader on actual access removal", async () => {
  const h = await fixture();
  const gate = h.hold();
  const work = h.session.inboxFeed.ensure();
  await gate.started.promise;
  try {
    render(
      <StrictMode>
        <Visit h={h} />
      </StrictMode>,
    );
    expect(screen.getByText("Preview updating…")).toBeVisible();
    expect(screen.getByRole("button", { name: "Close detail" })).toHaveFocus();
    expect(screen.queryByRole("complementary", { hidden: true })).toBeNull();
    expect(screen.queryByRole("textbox", { hidden: true })).toBeNull();
    await act(async () => {
      gate.gate.resolve();
      await work;
    });
    const editor = await screen.findByRole("textbox");
    const reader = screen.getByRole("complementary", { name: "Thread" });
    const pending = h.hold();
    let refresh!: Promise<void>;
    try {
      await act(async () => {
        refresh = h.session.inboxFeed.refresh();
        await pending.started.promise;
      });
      expect(reader).toBeInTheDocument();
      expect(reader).not.toBeVisible();
      act(() => h.revoke());
      expect(reader).not.toBeInTheDocument();
      expect(editor).not.toBeInTheDocument();
      expect(screen.queryByRole("textbox", { hidden: true })).toBeNull();
      await act(async () => {
        pending.gate.resolve();
        await refresh;
      });
      expect(
        screen.getByText(/This conversation is unavailable/),
      ).toBeVisible();
      expect(screen.queryByRole("textbox", { hidden: true })).toBeNull();
    } finally {
      pending.gate.resolve();
    }
  } finally {
    gate.gate.resolve();
  }
});

it("does not earn read dwell while a retained reader is hidden", async () => {
  const h = await fixture();
  const { reader, row } = await opened(h);
  const sender = screen.getByRole("combobox", { name: "Sender" });
  act(() => sender.focus()); // Cancel the opening dwell before controlling time.
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  act(() => (row as HTMLElement).focus());
  const before = h.journal()?.state.frontiers;
  expect(h.session.unread.sync().capability).toBe("frontier-sync");
  expect(before).toEqual({});
  const gate = h.hold();
  let work!: Promise<void>;
  try {
    await act(async () => {
      work = h.session.inboxFeed.refresh();
      await gate.started.promise;
    });
    expect(reader).toBeInTheDocument();
    expect(reader.closest("[hidden][inert]")).not.toBeNull();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(300);
    });
    expect(h.journal()?.state.frontiers).toEqual(before);
    await act(async () => {
      gate.gate.resolve();
      await work;
    });
    // Same real read hook can earn dwell again only once the surface is exposed.
    const scroller = reader.querySelector("[data-message-scroller]");
    if (!scroller) throw new Error("Missing reading scroller");
    fireEvent.scroll(scroller);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(300);
    });
    expect(h.journal()?.state.frontiers[`msg:${h.root.id}`]).toBe(20);
  } finally {
    gate.gate.resolve();
  }
});

it.each([false, true])(
  "hands focused composing to Close before paint and restores the retained editor (failure: %s)",
  async (fail) => {
    const h = await fixture();
    const { reader, editor } = await opened(h);
    const reveals = vi.mocked(HTMLElement.prototype.scrollIntoView).mock.calls
      .length;
    act(() => {
      editor.focus();
      editor.value = "Retained reply";
      editor.setSelectionRange(14, 14);
    });
    fireEvent.input(editor);
    const wrapper = reader.parentElement;
    expect(wrapper).not.toBeNull();
    const beforeMutation: (Element | null)[] = [];
    const handoffFrom: (Element | null)[] = [];
    const focus = HTMLElement.prototype.focus;
    vi.spyOn(HTMLElement.prototype, "focus").mockImplementation(function (
      this: HTMLElement,
      options,
    ) {
      if (this.closest("[hidden], [inert]")) return;
      if (this.getAttribute("aria-label") === "Close detail")
        handoffFrom.push(document.activeElement);
      focus.call(this, options);
    });
    // jsdom does not blur native hidden/inert descendants. Force that loss at
    // the actual React host mutation, before layout effects can inspect focus.
    const setAttribute = Element.prototype.setAttribute;
    vi.spyOn(Element.prototype, "setAttribute").mockImplementation(function (
      this: Element,
      name,
      value,
    ) {
      if (this === wrapper && name === "hidden") {
        beforeMutation.push(document.activeElement);
        setAttribute.call(this, name, value);
        editor.blur();
        expect(document.activeElement).toBe(document.body);
        return;
      }
      setAttribute.call(this, name, value);
    });
    const gate = h.hold(fail);
    try {
      act(() => {
        h.live.state({ status: "retrying", routes: [] });
        h.live.state({ status: "connected", routes: [] });
        h.live.established();
      });
      await act(async () => {
        await gate.started.promise;
      });
      expect(beforeMutation).toEqual([editor]);
      expect(handoffFrom).toEqual([document.body]);
      const close = screen.getByRole("button", { name: "Close detail" });
      expect(close).toBeVisible();
      expect(close).toHaveFocus();
      expect(reader).not.toBeVisible();
      expect(editor).toBeInTheDocument();
      await act(async () => {
        gate.gate.resolve();
      });
      if (fail) {
        expect(
          screen.getByText("Preview unavailable. Retry inbox."),
        ).toBeVisible();
        expect(close).toHaveFocus();
        gate.fail = false;
        await act(async () => {
          await h.session.inboxFeed.refresh();
        });
      }
      expect(reader).toBeVisible();
      expect(screen.getByRole("textbox")).toBe(editor);
      expect(editor).toHaveValue("Retained reply");
      expect(editor).toHaveFocus();
      act(() => {
        editor.value += " continued";
      });
      fireEvent.input(editor);
      expect(editor).toHaveValue("Retained reply continued");
      await frame();
      await frame();
      await frame();
      expect(editor).toHaveFocus();
      expect(HTMLElement.prototype.scrollIntoView).toHaveBeenCalledTimes(
        reveals,
      );
    } finally {
      gate.gate.resolve();
    }
  },
);

it.each(["outside", "blur", "other detail control", "away and back"])(
  "cancels withheld restoration after %s",
  async (move) => {
    const h = await fixture();
    const { reader, editor } = await opened(h);
    act(() => editor.focus());
    const gate = h.hold();
    let work!: Promise<void>;
    try {
      await act(async () => {
        work = h.session.inboxFeed.refresh();
        await gate.started.promise;
      });
      const close = screen.getByRole("button", { name: "Close detail" });
      expect(close).toHaveFocus();
      const sender = screen.getByRole("combobox", { name: "Sender" });
      const other = screen.getByRole("button", { name: "Open in channel" });
      act(() => {
        if (move === "blur") close.blur();
        else if (move === "other detail control") other.focus();
        else sender.focus();
        if (move === "away and back") close.focus();
      });
      await act(async () => {
        gate.gate.resolve();
        await work;
      });
      expect(reader).toBeVisible();
      expect(editor).not.toHaveFocus();
      if (move === "outside") expect(sender).toHaveFocus();
      else if (move === "other detail control") expect(other).toHaveFocus();
      else if (move === "away and back") expect(close).toHaveFocus();
      else expect(document.body).toHaveFocus();
    } finally {
      gate.gate.resolve();
    }
  },
);

it.each(["Escape", "Close", "retarget", "access removal"])(
  "retires withheld focus ownership on %s",
  async (exit) => {
    const h = await fixture();
    const { editor } = await opened(h);
    act(() => editor.focus());
    const gate = h.hold();
    let work!: Promise<void>;
    try {
      await act(async () => {
        work = h.session.inboxFeed.refresh();
        await gate.started.promise;
      });
      const close = screen.getByRole("button", { name: "Close detail" });
      expect(close).toHaveFocus();
      if (exit === "Escape") fireEvent.keyDown(close, { key: "Escape" });
      else if (exit === "Close") fireEvent.click(close);
      else if (exit === "retarget")
        fireEvent.click(screen.getByRole("button", { name: "Retarget" }));
      else act(() => h.revoke());
      expect(editor).not.toBeInTheDocument();
      const focus = vi.spyOn(editor, "focus");
      await act(async () => {
        gate.gate.resolve();
        await work;
      });
      await frame();
      expect(focus).not.toHaveBeenCalled();
      if (exit === "Escape" || exit === "Close")
        expect(
          screen.queryByRole("region", { name: "Inbox detail" }),
        ).toBeNull();
    } finally {
      gate.gate.resolve();
    }
  },
);

it.each(["Sender", "intentional blur"])(
  "does not take focus moved to %s before withholding",
  async (move) => {
    const h = await fixture();
    const { editor, reader } = await opened(h);
    const sender = screen.getByRole("combobox", { name: "Sender" });
    act(() => {
      editor.focus();
      if (move === "Sender") sender.focus();
      else editor.blur();
    });
    const expected = document.activeElement;
    const gate = h.hold();
    let work!: Promise<void>;
    try {
      await act(async () => {
        work = h.session.inboxFeed.refresh();
        await gate.started.promise;
      });
      expect(reader).not.toBeVisible();
      expect(document.activeElement).toBe(expected);
      await act(async () => {
        gate.gate.resolve();
        await work;
      });
      expect(reader).toBeVisible();
      expect(document.activeElement).toBe(expected);
    } finally {
      gate.gate.resolve();
    }
  },
);

it("does not restore a control retired by withholding, even when a replacement mounts", async () => {
  const h = await fixture();
  await opened(h);
  const toggle = screen.getByRole("button", { name: "Toggle formatting" });
  act(() => toggle.focus());
  const gate = h.hold();
  let work!: Promise<void>;
  try {
    await act(async () => {
      work = h.session.inboxFeed.refresh();
      await gate.started.promise;
    });
    expect(toggle).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Close detail" })).toHaveFocus();
    const focus = vi.spyOn(toggle, "focus");
    await act(async () => {
      gate.gate.resolve();
      await work;
    });
    expect(focus).not.toHaveBeenCalled();
    const replacement = screen.getByRole("button", {
      name: "Toggle formatting",
    });
    expect(replacement).not.toBe(toggle);
    expect(replacement).not.toHaveFocus();
  } finally {
    gate.gate.resolve();
  }
});

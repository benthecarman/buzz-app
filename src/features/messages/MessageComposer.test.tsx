import { snapshotClipboardHtml } from "../agents/snapshot-link";
// @vitest-environment jsdom
import { File as NodeFile } from "node:buffer";
import { createMemberAdditions } from "../channel-members/operations";
import { addChannelMember } from "../channel-members/members";
import "@testing-library/jest-dom/vitest";
import { composerDOMFixture } from "./composer-testing";
import { bindNames } from "../identity-names/service";
import { createAgentDirectory } from "../identity-names/testing";
import {
  afterEach,
  beforeEach,
  describe,
  expect,
  it,
  onTestFinished,
  vi,
} from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  within,
  waitFor,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useLayoutEffect } from "react";
import type { Contribution } from "../../plugins/contributions";
import type {
  ComposerCompletion,
  ComposerCompletionProps,
  ComposerTool,
  ComposerToolProps,
  CompletionResult,
  InlineRenderer,
} from "../conversation/contracts";
import type { AgentLibrarySnapshot } from "../agents/library";
import { createAgentChoices } from "../agents/choices";
import { createAgentControl, type AgentControl } from "../agents/control";
import { controlFixture } from "../agents/control-testing";
import { UploadError, UPLOAD_FAILURES } from "../relay/attachments";
import { ToastProvider } from "../../shared/design-system/ui/Toast";
import type { OutgoingEvent } from "../relay/outbox";
import { ConversationPresentation } from "../conversation/ConversationPresentation";
import { MessageComposer, type MessageComposerProps } from "./MessageComposer";
import { createRelaySession, type RelaySession } from "../relay/session";
import { keypair, metadata, roster, signed } from "../relay/testing";
import type { EventTemplate } from "nostr-tools";
import type { RelayEvent } from "../relay/events";
import type {
  ChannelMessage,
  ChannelSummary,
  Profile,
} from "../relay/contracts";
import { readView, writeView } from "../../shared/view-state";
import { emojiMatches, type CustomEmoji } from "../relay/emoji";
import { CustomEmoji as CustomEmojiImage } from "../../bundled/emoji/CustomEmoji";
import { EmojiCompletion } from "../../bundled/emoji/EmojiCompletion";
import * as emojiSearch from "../../bundled/emoji/emoji-search";
import type { EmojiSnapshot } from "../relay/emoji-directory";
import { emojiQuery } from "../../bundled/emoji/emoji-query";
import type { ComposerInputElement } from "./composer-dom";
import { profileTarget } from "../profiles/target";
import { fromMarkdown } from "mdast-util-from-markdown";
import { setRememberAgentsPreference } from "./mention-preferences";
import { ResourcePicker } from "../../bundled/projects/ResourcePicker";
import { entityHref } from "../projects/routes";
import type { Entity } from "../projects/destinations";

composerDOMFixture();

const owners: ReturnType<typeof createRelaySession>[] = [];

const first = { pubkey: "a".repeat(64), name: "Honey" };
const second = { pubkey: "b".repeat(64), name: "Honey" };
const resourceOwner = "c".repeat(64);
const resourceRoute = {
  type: "issue",
  owner: resourceOwner,
  dtag: "game",
  id: "d".repeat(64),
} as const;
const resource = { uri: entityHref(resourceRoute), label: "Fix login" };

beforeEach(() => {
  localStorage.clear();
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  HTMLElement.prototype.scrollIntoView = vi.fn();
});
afterEach(() => {
  cleanup();
  for (const owner of owners.splice(0)) owner.dispose();
  vi.unstubAllGlobals();
  delete (HTMLElement.prototype as Partial<HTMLElement>).scrollIntoView;
});

function mount(
  options: Partial<MessageComposerProps> = {},
  control?: AgentControl,
  viewer?: string,
) {
  let commands: ComposerToolProps;
  const completionRequests: ComposerCompletionProps["publish"][] = [];
  function Completion({ publish }: ComposerCompletionProps) {
    useLayoutEffect(() => {
      completionRequests.push(publish);
    }, [publish]);
    return null;
  }
  const completionListeners = new Set<() => void>();
  const completion = (revision: string): Contribution<ComposerCompletion> => ({
    id: "delayed",
    key: "test/delayed",
    pluginId: "test",
    revision,
    title: "Delayed",
    match: ({ text, start }) =>
      text.startsWith("!") && start > 0
        ? { start: 0, end: start, query: text.slice(1, start) }
        : null,
    component: Completion,
  });
  let completions: readonly Contribution<ComposerCompletion>[] = [
    completion("1"),
  ];
  function Tool(props: ComposerToolProps) {
    useLayoutEffect(() => {
      commands = props;
    });
    return (
      <>
        <button type="button" onClick={() => props.insertMention(first)}>
          First Honey
        </button>
        <button type="button" onClick={() => props.insertMention(second)}>
          Second Honey
        </button>
      </>
    );
  }
  const tools: readonly Contribution<ComposerTool>[] = [
    {
      id: "fixture",
      key: "test/fixture",
      pluginId: "test",
      revision: "1",
      title: "Fixture tools",
      component: Tool,
    },
  ];
  const emojiListeners = new Set<() => void>();
  let emoji: Pick<EmojiSnapshot, "status" | "entries"> = {
    status: "ready",
    entries: [],
  };
  const outboxListeners = new Set<() => void>();
  let pending: readonly OutgoingEvent[] = [];
  let rows: readonly ChannelMessage[] = [];
  const rowListeners = new Set<() => void>();
  const setPending = (next: readonly OutgoingEvent[]) => {
    pending = next;
    for (const listener of outboxListeners) listener();
  };
  const messages = {
    edit: vi.fn<RelaySession["messages"]["edit"]>((id, content) => {
      setPending([
        {
          event: {
            id: "edit-id",
            kind: 40003,
            pubkey: first.pubkey,
            created_at: 100,
            content,
            tags: [
              ["h", "channel"],
              ["e", id],
            ],
          },
          delivery: "sending",
        },
      ]);
      return "edit-id";
    }),
    send: vi.fn<RelaySession["messages"]["send"]>(() => "channel-id"),
    reply: vi.fn<RelaySession["messages"]["reply"]>(() => "reply-id"),
  };
  const typing: ReturnType<RelaySession["typing"]["snapshot"]> = [];
  let profiles: ReadonlyMap<string, Profile> = new Map();
  const profileListeners = new Set<() => void>();
  const libraryListeners = new Set<() => void>();
  let library: AgentLibrarySnapshot = {
    status: "ready",
    identities: [],
    definitions: [],
  };
  const channelList = {
    status: "ready" as const,
    channels: [{ id: "channel", members: [first.pubkey, second.pubkey] }],
  };
  const rawSession = {
    viewer,
    messages,
    typing: { snapshot: () => typing, subscribe: () => () => {} },
    profiles: {
      snapshot: () => profiles,
      subscribe(listener: () => void) {
        profileListeners.add(listener);
        return () => {
          profileListeners.delete(listener);
        };
      },
      ensure: vi.fn(async () => {}),
    },
    agentLibrary: {
      snapshot: () => library,
      subscribe(listener: () => void) {
        libraryListeners.add(listener);
        return () => libraryListeners.delete(listener);
      },
      refresh: vi.fn(async () => {}),
      retain: () => () => {},
    },
    emoji: {
      snapshot: () => emoji,
      subscribe(listener: () => void) {
        emojiListeners.add(listener);
        return () => {
          emojiListeners.delete(listener);
        };
      },
      ensure: vi.fn(() => Promise.resolve()),
      refresh: vi.fn(() => Promise.resolve()),
    },
    media: (url: string) => url,
    outbox: {
      supports: () => true,
      snapshot: () => pending,
      subscribe(listener: () => void) {
        outboxListeners.add(listener);
        return () => outboxListeners.delete(listener);
      },
      retry: vi.fn((id: string) =>
        setPending(
          pending.map((item) =>
            item.event.id === id
              ? { ...item, delivery: "sending", error: undefined }
              : item,
          ),
        ),
      ),
    },
    channels: {
      window: () => ({ rows }),
      subscribeWindow(_id: string, listener: () => void) {
        rowListeners.add(listener);
        return () => rowListeners.delete(listener);
      },
      list: () => channelList,
      subscribeList: () => () => {},
    },
  } as unknown as RelaySession;
  const session = {
    ...rawSession,
    names: bindNames(rawSession, {
      snapshot: () => [createAgentDirectory()],
      subscribe: () => () => {},
    }),
  };
  const onSend = vi.fn();
  const inline: readonly Contribution<InlineRenderer>[] = [
    {
      id: "emoji",
      key: "test/emoji",
      pluginId: "test",
      revision: "1",
      title: "Emoji",
      matches: ({ text, message }) => [
        ...emojiMatches(text, message.emoji ?? []),
      ],
      component: ({ text, content, media }) => {
        const entry = content.message.emoji?.find(
          (entry) => `:${entry.shortcode}:` === text.toLowerCase(),
        );
        return entry ? <CustomEmojiImage emoji={entry} media={media} /> : text;
      },
    },
  ];
  let props: MessageComposerProps = {
    session,
    onSend,
    scope: "scope",
    channelId: "channel",
    channelName: "General",
    extensions: {
      tools: { snapshot: () => tools, subscribe: () => () => {} },
      inline: { snapshot: () => inline, subscribe: () => () => {} },
      completions: {
        snapshot: () => completions,
        subscribe(listener) {
          completionListeners.add(listener);
          return () => completionListeners.delete(listener);
        },
      },
    },
    ...options,
  };
  const bindChoices = () => {
    const library = props.session.agentLibrary;
    props = {
      ...props,
      session: {
        ...props.session,
        scope: props.scope,
        agentChoices: createAgentChoices({
          scope: props.scope,
          library: { ...library, retain: () => () => {} },
          native: control,
          signal: new AbortController().signal,
        }),
      },
    };
  };
  bindChoices();
  let presented = true;
  const tree = () => (
    <ConversationPresentation value={presented}>
      <div hidden={!presented} inert={!presented}>
        <MessageComposer {...props} />
      </div>
    </ConversationPresentation>
  );
  const view = render(tree(), {
    reactStrictMode: true,
  });
  const input = () =>
    within(view.container).getByRole<ComposerInputElement>("textbox");
  return {
    ...view,
    input,
    messages,
    present(active: boolean) {
      presented = active;
      view.rerender(tree());
    },
    setRows(next: readonly ChannelMessage[]) {
      act(() => {
        rows = next;
        for (const listener of rowListeners) listener();
      });
    },
    setDelivery(delivery: OutgoingEvent["delivery"]) {
      act(() =>
        setPending(
          pending.map((item) => ({
            ...item,
            delivery,
            error:
              delivery === "failed" ? "Relay rejected this edit" : undefined,
          })),
        ),
      );
    },
    onSend,
    session: props.session,
    emojiListeners,
    user: userEvent.setup(),
    commands: () => commands,
    completionRequests,
    publish(index: number, text = "chosen") {
      const request = completionRequests[index];
      if (!request) throw new Error("No observed completion request");
      const result: CompletionResult = {
        items: [{ id: text, label: text, edit: { text } }],
      };
      let published: ReturnType<ComposerCompletionProps["publish"]> = false;
      act(() => {
        published = request(result);
      });
      return published;
    },
    replaceCompletionProvider(pluginId = "test") {
      act(() => {
        completions = [{ ...completion("2"), pluginId }];
        for (const listener of completionListeners) listener();
      });
    },
    retarget(next: Partial<MessageComposerProps>) {
      const changedSession =
        (next.session !== undefined && next.session !== props.session) ||
        (next.scope !== undefined && next.scope !== props.scope);
      props = { ...props, ...next };
      if (changedSession) bindChoices();
      view.rerender(tree());
    },
    setProfiles(next: ReadonlyMap<string, Profile>) {
      act(() => {
        profiles = next;
        for (const listener of profileListeners) listener();
      });
    },
    setLibrary(identities: AgentLibrarySnapshot["identities"]) {
      act(() => {
        library = { ...library, identities };
        for (const listener of libraryListeners) listener();
      });
    },
    setEmoji(
      entries: readonly CustomEmoji[],
      status: EmojiSnapshot["status"] = "ready",
    ) {
      act(() => {
        emoji = { status, entries };
        for (const listener of emojiListeners) listener();
      });
    },
    fill(text: string) {
      const field = input();
      act(() => {
        field.focus();
        field.value = text;
        field.setSelectionRange(text.length, text.length);
      });
      fireEvent.input(field);
      // Browsers queue selectionchange after the editor restores its native
      // selection. Deliver that boundary explicitly in this synchronous fixture.
      fireEvent(document, new Event("selectionchange"));
    },
    submit() {
      fireEvent.submit(within(view.container).getByRole("form"));
    },
  };
}

it("shows a local draft in the cached composer without completion, typing or transport reads", async () => {
  const viewer = keypair(),
    relay = keypair();
  const query = vi.fn(async () => []);
  const owner = createRelaySession(
    {
      viewer: viewer.pubkey,
      relayAuthor: relay.pubkey,
      query,
      media: () => undefined,
    },
    {
      cachedOnly: true,
      prepared: true,
      persistence: {
        readStartup: async () => ({
          discovery: {
            savedAt: Date.now(),
            relayAuthor: relay.pubkey,
            events: [
              roster(relay, "channel", [viewer.pubkey]),
              metadata(relay, "channel", "General"),
            ],
          },
        }),
        read: async () => [],
        write: async () => {},
        remove: async () => {},
        retain: async () => {},
        clear: async () => {},
        close() {},
      },
    },
  );
  await owner.restore();
  writeView("cached-composer", "draft:channel", "!Saved draft");
  const typing = vi.fn(owner.session.typing.subscribe);
  const h = mount({
    session: {
      ...owner.session,
      typing: { ...owner.session.typing, subscribe: typing },
    },
    scope: "cached-composer",
  });
  try {
    expect(h.input()).toHaveValue("!Saved draft");
    expect(h.input()).toHaveAttribute("aria-disabled", "true");
    expect(
      screen.queryByText(/does not support sending/),
    ).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    fireEvent.focus(h.input());
    fireEvent(document, new Event("selectionchange"));
    h.submit();
    await act(async () => {}); // Flush mounted effects before the negative assertions.
    expect(h.completionRequests).toEqual([]);
    expect(typing).not.toHaveBeenCalled();
    expect(query).not.toHaveBeenCalled();
    expect(h.input()).toHaveValue("!Saved draft");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  } finally {
    h.unmount();
    owner.dispose();
  }
});

it("autofocuses each selected conversation once without stealing focus on updates", () => {
  const h = mount({ autoFocus: true });
  expect(h.input()).toHaveFocus();
  const other = document.createElement("button");
  document.body.append(other);
  try {
    other.focus();
    h.retarget({ channelName: "Renamed", autoFocus: false });
    h.retarget({ autoFocus: true });
    expect(other).toHaveFocus();
    h.retarget({ channelId: "another-channel" });
    expect(h.input()).toHaveFocus();
    h.retarget({ channelId: "keyboard-navigation" });
    expect(h.input()).toHaveFocus();
  } finally {
    other.remove();
  }
});

it("defers initial focus until an inert startup ancestor is revealed", async () => {
  document.body.setAttribute("inert", "");
  try {
    const h = mount({ autoFocus: true });
    expect(h.input()).not.toHaveFocus();
    document.body.removeAttribute("inert");
    await waitFor(() => expect(h.input()).toHaveFocus());
  } finally {
    document.body.removeAttribute("inert");
  }
});

it("does not reclaim startup focus after another control takes it", async () => {
  document.body.setAttribute("inert", "");
  const other = document.createElement("button");
  document.body.append(other);
  try {
    const h = mount({ autoFocus: true });
    other.focus();
    document.body.removeAttribute("inert");
    await waitFor(() => expect(other).toHaveFocus());
    expect(h.input()).not.toHaveFocus();
  } finally {
    document.body.removeAttribute("inert");
    other.remove();
  }
});

it("does not take focus from a modal when the conversation mounts behind it", () => {
  const dialog = document.createElement("div");
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-modal", "true");
  const search = document.createElement("input");
  dialog.append(search);
  document.body.append(dialog);
  try {
    search.focus();
    mount({ autoFocus: true });
    expect(search).toHaveFocus();
  } finally {
    dialog.remove();
  }
});

it.each([
  ["an open popup trigger", { "data-popup-open": "" }],
  ["a menu", { role: "menu" }],
])("does not take focus from %s when the conversation mounts", (_, attrs) => {
  const owner = document.createElement("button");
  for (const [name, value] of Object.entries(attrs))
    owner.setAttribute(name, value);
  document.body.append(owner);
  try {
    owner.focus();
    mount({ autoFocus: true });
    expect(owner).toHaveFocus();
  } finally {
    owner.remove();
  }
});

it("restores the draft end through StrictMode replay without resetting a deliberate selection on updates", () => {
  writeView("scope", "draft:channel", "Saved draft");
  const h = mount({ autoFocus: true });
  expect(h.input()).toHaveFocus();
  expect(h.input().selectionStart).toBe("Saved draft".length);
  expect(h.input().selectionEnd).toBe("Saved draft".length);
  act(() => h.input().setSelectionRange(1, 4));
  h.retarget({ channelName: "Renamed" });
  expect(h.input().selectionStart).toBe(1);
  expect(h.input().selectionEnd).toBe(4);
  h.retarget({ channelId: "other" });
  h.retarget({ channelId: "channel" });
  expect(h.input()).toHaveFocus();
  expect(h.input().selectionStart).toBe("Saved draft".length);
});

it("lets an explicit focus restoration in the mount commit win", () => {
  const h = mount();
  h.unmount();
  const target = document.createElement("button");
  document.body.append(target);
  function RestoreFocus() {
    useLayoutEffect(() => target.focus(), []);
    return null;
  }
  try {
    render(
      <>
        <MessageComposer
          session={h.session}
          scope="scope"
          channelId="channel"
          channelName="General"
          autoFocus
        />
        <RestoreFocus />
      </>,
      { reactStrictMode: true },
    );
    expect(target).toHaveFocus();
  } finally {
    target.remove();
  }
});

it("leaves focus alone unless an enabled composer opts into mount focus", () => {
  const h = mount();
  expect(h.input()).not.toHaveFocus();
  h.retarget({
    channelId: "disabled-channel",
    disabled: true,
    autoFocus: true,
  });
  expect(h.input()).not.toHaveFocus();
});

it("keeps unpublished completions invisible but lets Escape revoke pending work", () => {
  const h = mount();
  const input = h.input();
  input.focus();
  h.fill("!pending");
  const pending = h.completionRequests.length - 1;
  expect(pending).toBeGreaterThanOrEqual(0);
  expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
  expect(screen.queryByRole("status", { name: "" })).not.toBeInTheDocument();
  expect(input).not.toHaveAttribute("aria-controls");
  expect(input).not.toHaveAttribute("aria-haspopup");
  fireEvent.keyDown(input, { key: "Escape" });
  expect(h.publish(pending, "late result")).toBe(false);
  expect(h.messages.send).not.toHaveBeenCalled();
  expect(input).toHaveValue("!pending");

  h.fill("!fresh");
  expect(h.publish(h.completionRequests.length - 1)).not.toBe(false);
  expect(screen.getByRole("option", { name: "chosen" })).toBeVisible();
  expect(input).toHaveAttribute("aria-controls");
});

it("shows provider-owned pending and retry states and hides an empty publication", () => {
  const h = mount();
  const input = h.input();
  input.focus();
  h.fill("!search");
  const publish = h.completionRequests.at(-1);
  if (!publish) throw new Error("No observed completion request");
  act(() => {
    publish({ items: [], status: "Searching fixture…" });
  });
  expect(screen.getByRole("status", { name: "" })).toHaveTextContent(
    "Searching fixture…",
  );
  const retry = vi.fn(() =>
    publish({
      items: [
        { id: "recovered", label: "Recovered", edit: { text: "recovered" } },
      ],
    }),
  );
  act(() => {
    publish({ items: [], status: "Unavailable", retry });
  });
  expect(
    screen.getByRole("option", { name: "Retry suggestions" }),
  ).toBeVisible();
  fireEvent.keyDown(input, { key: "Enter" });
  expect(retry).toHaveBeenCalledTimes(1);
  expect(screen.getByRole("option", { name: "Recovered" })).toBeVisible();
  expect(h.messages.send).not.toHaveBeenCalled();
  act(() => {
    publish({ items: [] });
  });
  expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
  expect(input).not.toHaveAttribute("aria-controls");
});

it.each(["click", "Enter", "Tab", " "])(
  "accepts the displayed completion by stable ID during a publication refresh via %s",
  (key) => {
    const h = mount();
    const input = h.input();
    input.focus();
    h.fill("!search");
    const publish = h.completionRequests.at(-1);
    if (!publish) throw new Error("No observed completion request");
    const other = { id: "other", label: "Other", edit: { text: "other" } };
    const chosen = { id: "chosen", label: "Chosen", edit: { text: "old" } };
    act(() => {
      publish({ items: [other, chosen], spaceId: chosen.id });
    });
    fireEvent.keyDown(input, { key: "ArrowDown" });
    const option = screen.getByRole("option", { name: "Chosen" });
    expect(option).toHaveAttribute("aria-selected", "true");

    act(() => {
      publish({
        items: [
          { ...chosen, label: "Refreshed", edit: { text: "fresh" } },
          other,
        ],
        spaceId: chosen.id,
      });
      // Hold React's commit until after the event, as when a provider's
      // passive effect publishes just before an input event is dispatched.
      expect(option).toHaveTextContent("Chosen");
      if (key === "click") fireEvent.click(option);
      else fireEvent.keyDown(input, { key });
    });
    expect(input).toHaveValue("fresh ");
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    expect(h.messages.send).not.toHaveBeenCalled();
  },
);

it.each(["click", "Enter", "Tab"])(
  "rejects withdrawn or ineligible refreshed completions via %s without sending",
  (key) => {
    const h = mount();
    const input = h.input();
    input.focus();
    h.fill("!search");
    const publish = h.completionRequests.at(-1);
    if (!publish) throw new Error("No observed completion request");
    const chosen = { id: "chosen", label: "Chosen", edit: { text: "old" } };
    const canSelect = vi.fn(() => false);
    const replacements: (CompletionResult | undefined)[] = [
      undefined,
      { items: [{ id: "other", label: "Other", edit: { text: "other" } }] },
      { items: [{ ...chosen, disabled: "No longer eligible" }] },
      { items: [{ ...chosen, canSelect }] },
    ];
    for (const replacement of replacements) {
      let withdraw: ReturnType<typeof publish> = false;
      act(() => {
        withdraw = publish({ items: [chosen] });
      });
      const option = screen.getByRole("option", { name: "Chosen" });
      act(() => {
        if (replacement) publish(replacement);
        else if (withdraw) withdraw();
        if (key === "click") fireEvent.click(option);
        else expect(fireEvent.keyDown(input, { key })).toBe(false);
      });
      expect(input).toHaveValue("!search");
      expect(h.messages.send).not.toHaveBeenCalled();
    }
    expect(canSelect).toHaveBeenCalledExactlyOnceWith(key);
  },
);

it.each([
  [" ", "test", undefined],
  [" ", "test", "other"],
  [":", "buzz.emoji", undefined],
  [":", "buzz.emoji", "other"],
])(
  "leaves %s alone for %s if the latest publication's exact match is %s",
  (key, pluginId, spaceId) => {
    const h = mount();
    h.replaceCompletionProvider(pluginId);
    const input = h.input();
    input.focus();
    h.fill("!search");
    const publish = h.completionRequests.at(-1);
    if (!publish) throw new Error("No observed completion request");
    const items = [
      { id: "chosen", label: "Chosen", edit: { text: "chosen" } },
      { id: "other", label: "Other", edit: { text: "other" } },
    ];
    act(() => {
      publish({ items, spaceId: "chosen" });
    });
    act(() => {
      publish({ items, spaceId });
      expect(fireEvent.keyDown(input, { key })).toBe(true);
    });
    expect(input).toHaveValue("!search");
    expect(h.messages.send).not.toHaveBeenCalled();
  },
);

it.each(["click", "Enter", "Tab"])(
  "uses only the current retry action during a publication refresh via %s",
  (key) => {
    const h = mount();
    const input = h.input();
    input.focus();
    h.fill("!search");
    const publish = h.completionRequests.at(-1);
    if (!publish) throw new Error("No observed completion request");
    const oldRetry = vi.fn();
    const retry = vi.fn();
    for (const currentRetry of [retry, undefined]) {
      act(() => {
        publish({ items: [], retry: oldRetry });
      });
      const option = screen.getByRole("option", { name: "Retry suggestions" });
      act(() => {
        // A new item at the displayed retry's index is not the user's choice.
        publish({
          items: [{ id: "other", label: "Other", edit: { text: "other" } }],
          ...(currentRetry ? { retry: currentRetry } : {}),
        });
        if (key === "click") fireEvent.click(option);
        else expect(fireEvent.keyDown(input, { key })).toBe(false);
      });
      expect(input).toHaveValue("!search");
      expect(oldRetry).not.toHaveBeenCalled();
      expect(h.messages.send).not.toHaveBeenCalled();
    }
    expect(retry).toHaveBeenCalledTimes(1);
  },
);

it("revokes stale completion publications across editor and ownership lifecycles and recovers freshly", () => {
  const h = mount();
  const input = h.input();
  input.focus();
  h.fill("!a");
  const edit = h.completionRequests.length - 1;
  h.fill("!b");
  h.fill("!a");
  expect(h.publish(edit, "stale ABA")).toBe(false);
  const afterAba = h.completionRequests.length - 1;
  expect(h.publish(afterAba)).not.toBe(false);
  expect(screen.getByRole("option", { name: "chosen" })).toBeVisible();

  fireEvent.keyDown(input, { key: "Escape" });
  expect(h.publish(afterAba, "stale dismissal")).toBe(false);
  expect(screen.queryByRole("listbox")).not.toBeInTheDocument();

  h.fill("!provider");
  const oldProvider = h.completionRequests.length - 1;
  h.replaceCompletionProvider();
  expect(h.publish(oldProvider, "stale provider")).toBe(false);
  const replacement = h.completionRequests.length - 1;
  expect(h.publish(replacement, "replacement fresh")).not.toBe(false);
  expect(
    screen.getByRole("option", { name: "replacement fresh" }),
  ).toBeVisible();

  h.fill("!destination");
  const oldDestination = h.completionRequests.length - 1;
  h.retarget({ threadRootId: "root" });
  expect(h.input()).toHaveValue("");
  expect(h.publish(oldDestination, "stale destination")).toBe(false);
  h.input().focus();
  h.fill("!fresh");
  const fresh = h.completionRequests.length - 1;
  expect(h.publish(fresh, "fresh recovery")).not.toBe(false);
  expect(screen.getByRole("option", { name: "fresh recovery" })).toBeVisible();

  h.unmount();
  expect(h.publish(fresh, "stale unmount")).toBe(false);
});

it.each(["disabled", "readOnly"] as const)(
  "rejects late and displayed completion results when the editor becomes %s",
  (state) => {
    const h = mount();
    const input = h.input();
    input.focus();
    h.fill("!late");
    const late = h.completionRequests.length - 1;
    if (state === "disabled") {
      h.retarget({ disabled: true });
      expect(input).toHaveAttribute("aria-disabled", "true");
      expect(input).toHaveAttribute("contenteditable", "false");
    } else input.readOnly = true;
    expect(h.publish(late, "late result")).toBe(false);

    if (state === "disabled") h.retarget({ disabled: false });
    else input.readOnly = false;
    expect(h.input().disabled).toBe(false);
    expect(h.input().readOnly).toBe(false);
    h.input().focus();
    h.fill("!displayed");
    const displayed = h.completionRequests.length - 1;
    expect(h.publish(displayed, "displayed choice")).not.toBe(false);
    expect(
      screen.getByRole("option", { name: "displayed choice" }),
    ).toBeVisible();
    if (state === "disabled") h.retarget({ disabled: true });
    else h.input().readOnly = true;
    fireEvent.keyDown(h.input(), { key: "Enter" });
    expect(h.input()).toHaveValue("!displayed");
    expect(h.messages.send).not.toHaveBeenCalled();
  },
);

function uploadDescriptor(name = "notes.txt") {
  return {
    name,
    url: `https://relay.example.test/media/${"a".repeat(64)}.txt`,
    type: "text/plain",
    size: 5,
    sha256: "a".repeat(64),
  };
}
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((done, fail) => {
    resolve = done;
    reject = fail;
  });
  return { promise, resolve, reject };
}
function attachmentFile(name = "notes.txt", bytes = "notes") {
  return new File([bytes], name, { type: "text/plain" });
}
function attachByPaste(target: HTMLElement, file = attachmentFile()) {
  fireEvent.paste(target, {
    clipboardData: { items: [{ kind: "file", getAsFile: () => file }] },
  });
}
function attachByDrop(target: HTMLElement, file = attachmentFile()) {
  const transfer = {
    types: ["Files"],
    files: [file],
    dropEffect: "uninitialized",
  };
  const over = new Event("dragover", { bubbles: true, cancelable: true });
  Object.defineProperty(over, "dataTransfer", { value: transfer });
  fireEvent(target, over);
  const drop = new Event("drop", { bubbles: true, cancelable: true });
  Object.defineProperty(drop, "dataTransfer", { value: transfer });
  fireEvent(target, drop);
}

async function mountUploadComposer(
  options: {
    threadRootId?: string;
    replyParentId?: string;
    publish?: (event: RelayEvent, signal?: AbortSignal) => Promise<void>;
    emojiRead?: () => Promise<RelayEvent[]>;
    editable?: boolean;
    media?: (url: string) => string | undefined;
    extensions?: MessageComposerProps["extensions"];
  } = {},
) {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  const viewer = keypair(),
    relay = keypair();
  const uploadCalls: {
    file: File;
    signal: AbortSignal;
    progress: ((sent: number, total: number) => void) | undefined;
    result: ReturnType<typeof deferred<ReturnType<typeof uploadDescriptor>>>;
  }[] = [];
  const sign = vi.fn(async (template: EventTemplate) =>
    signed(viewer, template),
  );
  const publish = vi.fn(
    options.publish ??
      (async (_event: RelayEvent, _signal?: AbortSignal) => {}),
  );
  const owner = createRelaySession(
    {
      viewer: viewer.pubkey,
      relayAuthor: relay.pubkey,
      scope: "https://relay.example.test",
      media: options.media ?? ((url) => url),
      uploadAttachment(file, signal, progress) {
        const result = deferred<ReturnType<typeof uploadDescriptor>>();
        uploadCalls.push({ file, signal, progress, result });
        return result.promise;
      },
      async query(filters) {
        if (
          filters.some((filter) => filter.kinds?.includes(30030)) &&
          options.emojiRead
        )
          return options.emojiRead();
        return filters.flatMap((filter) =>
          filter["#d"]?.includes("other")
            ? filter.kinds?.includes(39002)
              ? [roster(relay, "other", other.members, other.time)]
              : [metadata(relay, "other", "Random", other.time)]
            : filter.kinds?.includes(39002)
              ? [roster(relay, "channel", [viewer.pubkey], 1700000000)]
              : filter.kinds?.includes(39000)
                ? [metadata(relay, "channel", "General", 1700000000)]
                : [],
        );
      },
      writer: { kinds: options.editable ? [9, 40003, 5] : [9], sign, publish },
    },
    { outboxStorage: { load: () => [], save() {} } },
  );
  const other = { members: [] as string[], time: 1700000000 };
  /** Changes membership of an unrelated channel; losing it revokes access. */
  const otherMembership = (joined: boolean) => {
    other.members = joined ? [viewer.pubkey] : [];
    other.time++;
    return act(() =>
      owner.session.read([
        { kinds: [39002], "#d": ["other"], limit: 1 },
        { kinds: [39000], "#d": ["other"], limit: 1 },
      ]),
    );
  };
  await act(() =>
    owner.session.read([
      { kinds: [39002], "#d": ["channel"], limit: 1 },
      { kinds: [39000], "#d": ["channel"], limit: 1 },
    ]),
  );
  const scope = `https://relay.example.test:${viewer.pubkey}`;
  const view = render(
    <MessageComposer
      session={owner.session}
      scope={scope}
      channelId="channel"
      channelName="General"
      {...(options.extensions ? { extensions: options.extensions } : {})}
      {...(options.threadRootId ? { threadRootId: options.threadRootId } : {})}
      {...(options.replyParentId
        ? { replyParentId: options.replyParentId }
        : {})}
    />,
    { reactStrictMode: true, wrapper: ToastProvider },
  );
  const input = () => within(view.container).getByRole("textbox");
  const form = () => within(view.container).getByRole("form");
  const send = () =>
    within(view.container).getByRole("button", { name: "Send message" });
  owners.push(owner);
  return {
    ...view,
    owner,
    input,
    form,
    send,
    scope,
    uploadCalls,
    sign,
    publish,
    otherMembership,
  };
}

it("keeps picker, paste and drop attachments local until Send starts upload and publish", async () => {
  const h = await mountUploadComposer();
  const picker =
    h.container.querySelector<HTMLInputElement>('input[type="file"]');
  if (!picker) throw new Error("Missing attachment picker");
  const picked = attachmentFile("picker.txt");
  Object.defineProperty(picker, "files", {
    value: [picked],
    configurable: true,
  });
  fireEvent.change(picker);
  attachByPaste(h.input(), attachmentFile("pasted.txt"));
  attachByDrop(h.form(), attachmentFile("dropped.txt"));
  await waitFor(() =>
    expect(within(h.form()).getAllByText(/\.txt$/)).toHaveLength(3),
  );
  expect(h.uploadCalls).toHaveLength(0);
  expect(h.sign).not.toHaveBeenCalled();
  expect(h.publish).not.toHaveBeenCalled();

  fireEvent.click(h.send());
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  expect(h.uploadCalls[0]?.file.name).toBe("picker.txt");
  expect(screen.queryByText("Adding agent to this channel…")).toBeNull();
  // Send hands the files to the background upload and frees the composer.
  expect(within(h.form()).queryAllByText(/\.txt$/)).toHaveLength(0);
  expect(screen.getByText("Uploading", { exact: true })).toHaveAttribute(
    "role",
    "status",
  );
  const bar = screen.getByRole("progressbar", { name: "Uploading" });
  expect(h.input()).toBeEnabled();
  await act(async () => {
    h.uploadCalls[0]?.result.resolve(uploadDescriptor("picker.txt"));
  });
  await waitFor(() => expect(h.uploadCalls).toHaveLength(2));
  // Without byte reports, a finished file does not step the bar to 33%.
  expect(bar).not.toHaveAttribute("aria-valuenow");
  act(() => h.uploadCalls[1]?.progress?.(4, 5));
  // The third file is not prepared yet; its transfer total is unknown.
  expect(bar).not.toHaveAttribute("aria-valuenow");
  expect(screen.queryByText(/%|\d/)).toBeNull();
  await act(async () => {
    h.uploadCalls[1]?.result.resolve(uploadDescriptor("pasted.txt"));
  });
  await waitFor(() => expect(h.uploadCalls).toHaveLength(3));
  await act(async () => {
    h.uploadCalls[2]?.result.resolve(uploadDescriptor("dropped.txt"));
  });
  await waitFor(() => expect(h.publish).toHaveBeenCalledTimes(1));
  expect(h.sign).toHaveBeenCalledTimes(1);
  expect(h.publish.mock.calls[0]?.[0].content).toContain(
    "[picker.txt](<https://relay.example.test/media/",
  );
  expect(h.publish.mock.calls[0]?.[0].content).toContain("[pasted.txt](<");
  expect(h.publish.mock.calls[0]?.[0].content).toContain("[dropped.txt](<");
  expect(screen.queryByText(/^Uploading/)).toBeNull();
});

it("restores a failed background upload into the composer with Desktop's toast", async () => {
  const h = await mountUploadComposer();
  attachByPaste(h.input(), attachmentFile("metadata.txt"));
  await waitFor(() =>
    expect(within(h.form()).getByText("metadata.txt")).toBeVisible(),
  );
  await userEvent.type(h.input(), "caption");

  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  await act(async () => {
    h.uploadCalls[0]?.result.reject(new UploadError("metadata"));
  });
  await waitFor(() =>
    expect(
      screen.getAllByRole("alert").map((node) => node.textContent),
    ).toEqual([UPLOAD_FAILURES.metadata]),
  );
  expect(
    screen.getByText(`Upload failed: ${UPLOAD_FAILURES.metadata}`),
  ).toBeVisible();
  expect(h.input()).toHaveValue("caption");
  expect(h.publish).not.toHaveBeenCalled();

  await userEvent.click(
    screen.getByRole("button", { name: "Remove metadata.txt" }),
  );

  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(within(h.form()).queryByText("metadata.txt")).not.toBeInTheDocument();
});

it.each(["cleanup", "restore"] as const)(
  "retains a failed attachment send until %s storage recovers",
  async (phase) => {
    const h = await mountUploadComposer();
    attachByPaste(h.input(), attachmentFile("retained.txt"));
    await userEvent.type(h.input(), "recover this caption");
    const key = `buzz-view.v1:${JSON.stringify([h.scope, "draft:channel"])}`;
    const original = Storage.prototype.setItem;
    let failWrites = phase === "cleanup";
    const write = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation(function (this: Storage, name, value) {
        if (name === key && failWrites) throw Error("storage full");
        return original.call(this, name, value);
      });
    try {
      fireEvent.click(h.send());
      await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
      if (phase === "cleanup") {
        expect(h.input()).toHaveAttribute("contenteditable", "false");
        expect(readView(h.scope, "draft:channel", null)).toMatchObject({
          text: "recover this caption",
        });
      }
      if (phase === "restore") {
        expect(readView(h.scope, "draft:channel", null)).toMatchObject({
          text: "",
        });
        failWrites = true;
      }
      await act(async () =>
        h.uploadCalls[0]?.result.reject(new Error("upload unavailable")),
      );
      if (phase === "restore") {
        await screen.findByRole("button", {
          name: "Retry failed send recovery",
        });
        expect(h.input()).toHaveValue("");
        failWrites = false;
        await userEvent.click(
          screen.getByRole("button", { name: "Retry failed send recovery" }),
        );
      } else {
        await waitFor(() =>
          expect(h.input()).toHaveValue("recover this caption"),
        );
      }
      await waitFor(() =>
        expect(h.input()).toHaveValue("recover this caption"),
      );
      expect(within(h.form()).getByText("retained.txt")).toBeVisible();
      expect(
        screen.queryByRole("button", { name: "Retry draft cleanup" }),
      ).toBeNull();
      expect(h.publish).not.toHaveBeenCalled();
    } finally {
      write.mockRestore();
    }
  },
);

it("offers emoji catalog refresh after an attachment publication fails preparation", async () => {
  let available = false;
  const h = await mountUploadComposer({
    emojiRead: async () => {
      if (!available) throw Error("catalog offline");
      return [];
    },
  });
  await waitFor(() =>
    expect(h.owner.session.emoji.snapshot().status).toBe("error"),
  );
  attachByPaste(h.input(), attachmentFile("emoji.txt"));
  await userEvent.type(h.input(), "caption :party:");
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  await act(async () =>
    h.uploadCalls[0]?.result.resolve(uploadDescriptor("emoji.txt")),
  );
  await waitFor(() => expect(h.input()).toHaveValue("caption :party:"));
  expect(within(h.form()).getByRole("alert")).toHaveTextContent(
    "Community emoji unavailable",
  );
  expect(
    screen.getByRole("button", { name: "Retry message preparation" }),
  ).toBeVisible();
  available = true;
  fireEvent.click(
    screen.getByRole("button", { name: "Retry message preparation" }),
  );
  await waitFor(() =>
    expect(h.owner.session.emoji.snapshot().status).toBe("ready"),
  );
  expect(h.input()).toHaveValue("caption :party:");
  expect(within(h.form()).getByText("emoji.txt")).toBeVisible();
  expect(h.publish).not.toHaveBeenCalled();
});

it("does not overwrite a later local edit when a background upload fails", async () => {
  const h = await mountUploadComposer();
  attachByPaste(h.input(), attachmentFile("original.txt"));
  await userEvent.type(h.input(), "original caption");
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  await userEvent.type(h.input(), "later work");
  await act(async () => h.uploadCalls[0]?.result.reject(new Error("offline")));
  expect(h.input()).toHaveValue("later work");
  expect(readView(h.scope, "draft:channel", null)).toMatchObject({
    text: "later work",
  });
  expect(
    screen.getByText(/Failed send was kept because this draft changed/),
  ).toBeVisible();
  const retry = screen.getByRole("button", {
    name: "Retry failed send recovery",
  });
  await userEvent.click(retry);
  expect(h.input()).toHaveValue("later work");
  await userEvent.clear(h.input());
  await userEvent.click(retry);
  await waitFor(() => expect(h.input()).toHaveValue("original caption"));
  expect(within(h.form()).getByText("original.txt")).toBeVisible();
  expect(
    screen.queryByRole("button", { name: "Retry failed send recovery" }),
  ).toBeNull();
  expect(h.publish).not.toHaveBeenCalled();
});

it("restores a failed upload after the composer remounts in the same session", async () => {
  const h = await mountUploadComposer();
  attachByPaste(h.input(), attachmentFile("remount.txt"));
  await userEvent.type(h.input(), "remount caption");
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  h.unmount();
  const again = render(
    <MessageComposer
      session={h.owner.session}
      scope={h.scope}
      channelId="channel"
      channelName="General"
    />,
    { wrapper: ToastProvider },
  );
  await act(async () => h.uploadCalls[0]?.result.reject(new Error("offline")));
  await waitFor(() =>
    expect(within(again.container).getByRole("textbox")).toHaveValue(
      "remount caption",
    ),
  );
  expect(within(again.container).getByText("remount.txt")).toBeVisible();
});

it("reconciles failed-cleanup recovery after remount before stale cleanup can overwrite the caption", async () => {
  const h = await mountUploadComposer();
  attachByPaste(h.input(), attachmentFile("retained.txt"));
  await userEvent.type(h.input(), "recover this caption");
  const key = `buzz-view.v1:${JSON.stringify([h.scope, "draft:channel"])}`;
  const original = Storage.prototype.setItem;
  let failCleanup = true;
  const write = vi
    .spyOn(Storage.prototype, "setItem")
    .mockImplementation(function (this: Storage, name, value) {
      if (name === key && failCleanup) throw Error("storage full");
      return original.call(this, name, value);
    });
  try {
    fireEvent.click(h.send());
    await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
    expect(h.input()).toHaveAttribute("contenteditable", "false");
    h.unmount();
    const again = render(
      <MessageComposer
        session={h.owner.session}
        scope={h.scope}
        channelId="channel"
        channelName="General"
      />,
      { wrapper: ToastProvider },
    );
    const replacement = within(again.container).getByRole("textbox");
    expect(replacement).toHaveAttribute("contenteditable", "false");
    failCleanup = false;
    await act(async () =>
      h.uploadCalls[0]?.result.reject(new Error("offline")),
    );
    await waitFor(() =>
      expect(replacement).toHaveValue("recover this caption"),
    );
    expect(replacement).toHaveAttribute("contenteditable", "true");
    expect(within(again.container).getByText("retained.txt")).toBeVisible();
    expect(
      within(again.container).queryByRole("button", {
        name: "Retry draft cleanup",
      }),
    ).toBeNull();
    await userEvent.type(replacement, " again");
    expect(replacement).toHaveValue(" againrecover this caption");
    expect(readView(h.scope, "draft:channel", null)).toMatchObject({
      text: " againrecover this caption",
    });
    expect(h.publish).not.toHaveBeenCalled();
  } finally {
    write.mockRestore();
  }
});

it("retains a remounted later edit conflict until clearing it and explicitly retrying recovery", async () => {
  const h = await mountUploadComposer();
  attachByPaste(h.input(), attachmentFile("original.txt"));
  await userEvent.type(h.input(), "original caption");
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  h.unmount();
  const again = render(
    <MessageComposer
      session={h.owner.session}
      scope={h.scope}
      channelId="channel"
      channelName="General"
    />,
    { wrapper: ToastProvider },
  );
  const replacement = within(again.container).getByRole("textbox");
  await userEvent.type(replacement, "later edit");
  await act(async () => h.uploadCalls[0]?.result.reject(new Error("offline")));
  const retry = screen.getByRole("button", {
    name: "Retry failed send recovery",
  });
  expect(replacement).toHaveValue("later edit");
  await userEvent.click(retry);
  expect(replacement).toHaveValue("later edit");
  expect(within(again.container).queryByText("original.txt")).toBeNull();
  await userEvent.clear(replacement);
  expect(readView(h.scope, "draft:channel", null)).toMatchObject({ text: "" });
  await userEvent.click(retry);
  await waitFor(() => expect(replacement).toHaveValue("original caption"));
  expect(replacement).toHaveAttribute("contenteditable", "true");
  expect(within(again.container).getByText("original.txt")).toBeVisible();
  expect(
    screen.queryByRole("button", { name: "Retry failed send recovery" }),
  ).toBeNull();
  expect(h.publish).not.toHaveBeenCalled();
});

it("does not replace an active message edit when a remounted upload recovers", async () => {
  const h = await mountUploadComposer({ editable: true });
  attachByPaste(h.input(), attachmentFile("original.txt"));
  await userEvent.type(h.input(), "original caption");
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  h.unmount();
  const viewer = h.owner.session.viewer;
  if (!viewer) throw new Error("Expected upload viewer");
  const row = editableMessage({ authorId: viewer });
  const again = render(
    <MessageComposer
      session={h.owner.session}
      scope={h.scope}
      channelId="channel"
      channelName="General"
      editMessages={[row]}
    />,
    { wrapper: ToastProvider },
  );
  const replacement = within(again.container).getByRole("textbox");
  fireEvent.keyDown(replacement, { key: "ArrowUp" });
  expect(replacement).toHaveAccessibleName("Edit message");
  expect(replacement).toHaveValue("Original message");
  await act(async () => h.uploadCalls[0]?.result.reject(new Error("offline")));
  expect(replacement).toHaveAccessibleName("Edit message");
  expect(replacement).toHaveValue("Original message");
  expect(readView(h.scope, "draft:channel", null)).toMatchObject({
    text: "original caption",
  });
  fireEvent.click(
    within(again.container).getByRole("button", { name: "Cancel edit" }),
  );
  await waitFor(() => expect(replacement).toHaveValue("original caption"));
  expect(within(again.container).getByText("original.txt")).toBeVisible();
  expect(h.publish).not.toHaveBeenCalled();
});

it("defers a failed upload recovery until an empty active message edit ends", async () => {
  const h = await mountUploadComposer({ editable: true });
  attachByPaste(h.input(), attachmentFile("original.txt"));
  await userEvent.type(h.input(), "original caption");
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  const viewer = h.owner.session.viewer;
  if (!viewer) throw new Error("Expected upload viewer");
  const row = editableMessage({ authorId: viewer });
  h.rerender(
    <MessageComposer
      session={h.owner.session}
      scope={h.scope}
      channelId="channel"
      channelName="General"
      editMessages={[row]}
    />,
  );
  const editor = h.input();
  fireEvent.keyDown(editor, { key: "ArrowUp" });
  expect(editor).toHaveAccessibleName("Edit message");
  await userEvent.clear(editor);
  expect(editor).toHaveValue("");
  await act(async () => h.uploadCalls[0]?.result.reject(new Error("offline")));
  expect(editor).toHaveAccessibleName("Edit message");
  expect(editor).toHaveValue("");
  expect(within(h.form()).queryByText("original.txt")).toBeNull();
  expect(readView(h.scope, "draft:channel", null)).toMatchObject({
    text: "original caption",
  });
  fireEvent.click(
    within(h.form()).getByRole("button", { name: "Save changes" }),
  );
  expect(h.publish).not.toHaveBeenCalled();
  fireEvent.click(
    within(h.form()).getByRole("button", { name: "Cancel edit" }),
  );
  await waitFor(() => expect(editor).toHaveValue("original caption"));
  expect(within(h.form()).getByText("original.txt")).toBeVisible();
});

it("retains preparation retry when recovery is deferred by an edit then unmounted", async () => {
  const h = await mountUploadComposer({
    editable: true,
    emojiRead: async () => {
      throw Error("catalog offline");
    },
  });
  await waitFor(() =>
    expect(h.owner.session.emoji.snapshot().status).toBe("error"),
  );
  attachByPaste(h.input(), attachmentFile("edit-remount.txt"));
  await userEvent.type(h.input(), "caption :party:");
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  const viewer = h.owner.session.viewer;
  if (!viewer) throw new Error("Expected upload viewer");
  const row = editableMessage({ authorId: viewer });
  h.rerender(
    <MessageComposer
      session={h.owner.session}
      scope={h.scope}
      channelId="channel"
      channelName="General"
      editMessages={[row]}
    />,
  );
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  await userEvent.clear(h.input());
  await act(async () =>
    h.uploadCalls[0]?.result.resolve(uploadDescriptor("edit-remount.txt")),
  );
  expect(h.input()).toHaveAccessibleName("Edit message");
  expect(
    within(h.form()).queryByRole("button", {
      name: "Retry message preparation",
    }),
  ).toBeNull();
  h.unmount();
  const returned = render(
    <MessageComposer
      session={h.owner.session}
      scope={h.scope}
      channelId="channel"
      channelName="General"
      editMessages={[row]}
    />,
    { wrapper: ToastProvider },
  );
  const composer = within(returned.container);
  expect(composer.getByRole("textbox")).toHaveValue("caption :party:");
  expect(composer.getByText("edit-remount.txt")).toBeVisible();
  expect(composer.getByRole("alert")).toHaveTextContent(
    "Community emoji unavailable",
  );
  expect(
    composer.getByRole("button", { name: "Retry message preparation" }),
  ).toBeVisible();
});

it("offers message preparation retry after a failed attachment send remounts", async () => {
  let available = false;
  const h = await mountUploadComposer({
    emojiRead: async () => {
      if (!available) throw Error("catalog offline");
      return [];
    },
  });
  await waitFor(() =>
    expect(h.owner.session.emoji.snapshot().status).toBe("error"),
  );
  attachByPaste(h.input(), attachmentFile("emoji-remount.txt"));
  await userEvent.type(h.input(), "caption :party:");
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  h.unmount();
  await act(async () =>
    h.uploadCalls[0]?.result.resolve(uploadDescriptor("emoji-remount.txt")),
  );
  const again = render(
    <MessageComposer
      session={h.owner.session}
      scope={h.scope}
      channelId="channel"
      channelName="General"
    />,
    { wrapper: ToastProvider },
  );
  const composer = within(again.container);
  await waitFor(() =>
    expect(composer.getByRole("textbox")).toHaveValue("caption :party:"),
  );
  expect(composer.getByText("emoji-remount.txt")).toBeVisible();
  expect(composer.getByRole("alert")).toHaveTextContent(
    "Community emoji unavailable",
  );
  available = true;
  fireEvent.click(
    composer.getByRole("button", { name: "Retry message preparation" }),
  );
  await waitFor(() =>
    expect(h.owner.session.emoji.snapshot().status).toBe("ready"),
  );
  expect(composer.getByRole("textbox")).toHaveValue("caption :party:");
  expect(composer.getByText("emoji-remount.txt")).toBeVisible();
  expect(h.publish).not.toHaveBeenCalled();
  again.unmount();
  const returned = render(
    <MessageComposer
      session={h.owner.session}
      scope={h.scope}
      channelId="channel"
      channelName="General"
    />,
    { wrapper: ToastProvider },
  );
  expect(
    within(returned.container).queryByRole("button", {
      name: "Retry message preparation",
    }),
  ).toBeNull();
  expect(within(returned.container).getByRole("textbox")).toHaveValue(
    "caption :party:",
  );
});

it("keeps a remaining upload failure banner when removing one of two failed files", async () => {
  const h = await mountUploadComposer();
  attachByPaste(h.input(), attachmentFile("one.txt"));
  attachByPaste(h.input(), attachmentFile("two.txt"));
  await waitFor(() =>
    expect(within(h.form()).getAllByText(/\.txt$/)).toHaveLength(2),
  );

  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  await act(async () => {
    h.uploadCalls[0]?.result.reject(new Error("first failed"));
  });
  await waitFor(() =>
    expect(
      screen.getAllByRole("alert").map((node) => node.textContent),
    ).toEqual(["first failed"]),
  );
  await userEvent.click(screen.getByRole("button", { name: "Retry one.txt" }));
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(2));
  await act(async () => {
    h.uploadCalls[1]?.result.reject(new Error("first failed again"));
  });
  await waitFor(() =>
    expect(
      screen.getAllByRole("alert").map((node) => node.textContent),
    ).toEqual(["first failed again"]),
  );
  await userEvent.click(screen.getByRole("button", { name: "Retry one.txt" }));
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(3));
  await act(async () => {
    h.uploadCalls[2]?.result.resolve(uploadDescriptor("one.txt"));
  });
  await waitFor(() => expect(h.uploadCalls).toHaveLength(4));
  await act(async () => {
    h.uploadCalls[3]?.result.reject(new Error("second failed"));
  });
  await waitFor(() =>
    expect(
      screen.getAllByRole("alert").map((node) => node.textContent),
    ).toEqual(["second failed"]),
  );

  await userEvent.click(screen.getByRole("button", { name: "Remove one.txt" }));

  expect(screen.getAllByRole("alert").map((node) => node.textContent)).toEqual([
    "second failed",
  ]);
  expect(within(h.form()).queryByText("one.txt")).not.toBeInTheDocument();
  expect(within(h.form()).getByText("two.txt")).toBeVisible();
});

it("retains successful attachment uploads after a later file fails and retries only failed bytes", async () => {
  const h = await mountUploadComposer();
  attachByPaste(h.input(), attachmentFile("ready.txt"));
  attachByPaste(h.input(), attachmentFile("retry.txt"));
  await waitFor(() =>
    expect(within(h.form()).getAllByText(/\.txt$/)).toHaveLength(2),
  );
  await userEvent.type(h.input(), "caption");
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  await act(async () => {
    h.uploadCalls[0]?.result.resolve(uploadDescriptor("ready.txt"));
  });
  await waitFor(() => expect(h.uploadCalls).toHaveLength(2));
  await act(async () => {
    h.uploadCalls[1]?.result.reject(new Error("relay down"));
  });
  await screen.findAllByRole("alert");
  expect(screen.getAllByRole("alert").map((node) => node.textContent)).toEqual([
    "relay down",
  ]);
  expect(screen.getByText("Upload failed: relay down")).toBeVisible();
  expect(h.input()).toHaveValue("caption");
  expect(h.publish).not.toHaveBeenCalled();

  await userEvent.click(screen.getByRole("button", { name: /Retry/ }));
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(within(h.form()).getByText(/Queued/)).toBeVisible();
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(3));
  expect(h.uploadCalls[2]?.file.name).toBe("retry.txt");
  await act(async () => {
    h.uploadCalls[2]?.result.resolve(uploadDescriptor("retry.txt"));
  });
  await waitFor(() => expect(h.publish).toHaveBeenCalledTimes(1));
  expect(h.publish.mock.calls[0]?.[0].content).toContain("caption");
  expect(h.publish.mock.calls[0]?.[0].content).toContain("[ready.txt](<");
  expect(h.publish.mock.calls[0]?.[0].content).toContain("[retry.txt](<");
});

it("finishes a background send in its original channel after the composer unmounts", async () => {
  const h = await mountUploadComposer();
  attachByPaste(h.input(), attachmentFile("ready.txt"));
  attachByPaste(h.input(), attachmentFile("late.txt"));
  await waitFor(() =>
    expect(within(h.form()).getAllByText(/\.txt$/)).toHaveLength(2),
  );
  await userEvent.type(h.input(), "caption");
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  await act(async () => {
    h.uploadCalls[0]?.result.resolve(uploadDescriptor("ready.txt"));
  });
  await waitFor(() => expect(h.uploadCalls).toHaveLength(2));

  h.unmount();
  expect(h.uploadCalls[1]?.signal.aborted).toBe(false);
  await act(async () => {
    h.uploadCalls[1]?.result.resolve(uploadDescriptor("late.txt"));
  });
  await waitFor(() => expect(h.publish).toHaveBeenCalledTimes(1));
  const event = h.publish.mock.calls[0]?.[0];
  expect(event?.tags).toContainEqual(["h", "channel"]);
  expect(event?.content).toContain("caption");
  expect(event?.content).toContain("[ready.txt](<");
  expect(event?.content).toContain("[late.txt](<");
  expect(h.sign).toHaveBeenCalledTimes(1);
});

it("keeps a background send on its original thread when the composer moves", async () => {
  const root = "b".repeat(64);
  const h = await mountUploadComposer({ threadRootId: root });
  attachByPaste(h.input(), attachmentFile());
  await waitFor(() => expect(h.send()).toBeEnabled());
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  h.rerender(
    <MessageComposer
      session={h.owner.session}
      scope={`https://relay.example.test:${h.owner.session.viewer}`}
      channelId="channel"
      channelName="General"
      threadRootId={"c".repeat(64)}
    />,
  );
  expect(h.uploadCalls[0]?.signal.aborted).toBe(false);
  await act(async () => {
    h.uploadCalls[0]?.result.resolve(uploadDescriptor());
  });
  await waitFor(() => expect(h.publish).toHaveBeenCalledTimes(1));
  const tags = h.publish.mock.calls[0]?.[0].tags ?? [];
  expect(tags).toContainEqual(["e", root, "", "reply"]);
  expect(tags.flat()).not.toContain("c".repeat(64));
});

it("keeps a background reply on the parent captured at Send", async () => {
  const root = "a".repeat(64);
  const child = "b".repeat(64);
  const h = await mountUploadComposer({
    threadRootId: root,
    replyParentId: child,
  });
  attachByPaste(h.input(), attachmentFile("reply.txt"));
  await waitFor(() => expect(h.send()).toBeEnabled());
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  h.rerender(
    <MessageComposer
      session={h.owner.session}
      scope={`https://relay.example.test:${h.owner.session.viewer}`}
      channelId="channel"
      channelName="General"
      threadRootId={root}
    />,
  );
  expect(h.uploadCalls[0]?.signal.aborted).toBe(false);
  await act(async () => {
    h.uploadCalls[0]?.result.resolve(uploadDescriptor("reply.txt"));
  });
  await waitFor(() => expect(h.publish).toHaveBeenCalledTimes(1));
  expect(h.publish.mock.calls[0]?.[0].tags).toContainEqual([
    "e",
    child,
    "",
    "reply",
  ]);
});

it("blocks Send in a conversation until its background send settles", async () => {
  const h = await mountUploadComposer();
  attachByPaste(h.input(), attachmentFile("one.txt"));
  await userEvent.type(h.input(), "look at this");
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  expect(h.input()).toHaveValue("");

  // Composition stays open; only Send waits, so the text cannot overtake.
  await userEvent.type(h.input(), "thoughts?");
  expect(h.input()).toHaveValue("thoughts?");
  expect(h.send()).toBeDisabled();
  fireEvent.submit(h.form());
  await act(async () => {});
  expect(h.publish).not.toHaveBeenCalled();

  await act(async () => {
    h.uploadCalls[0]?.result.resolve(uploadDescriptor("one.txt"));
  });
  await waitFor(() => expect(h.publish).toHaveBeenCalledTimes(1));
  await waitFor(() => expect(h.send()).toBeEnabled());
  fireEvent.click(h.send());
  await waitFor(() => expect(h.publish).toHaveBeenCalledTimes(2));
  expect(h.publish.mock.calls[0]?.[0].content).toContain("look at this");
  expect(h.publish.mock.calls[1]?.[0].content).toBe("thoughts?");
});

it("cancels the newest background send, restores its draft and leaves older sends running", async () => {
  const root = "b".repeat(64);
  const h = await mountUploadComposer();
  attachByPaste(h.input(), attachmentFile("one.txt"));
  await userEvent.type(h.input(), "first");
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));

  h.rerender(
    <MessageComposer
      session={h.owner.session}
      scope={`https://relay.example.test:${h.owner.session.viewer}`}
      channelId="channel"
      channelName="General"
      threadRootId={root}
    />,
  );
  attachByPaste(h.input(), attachmentFile("two.txt"));
  await userEvent.type(h.input(), "second");
  await waitFor(() => expect(h.send()).toBeEnabled());
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(2));
  expect(h.input()).toHaveValue("");

  await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
  expect(h.uploadCalls[1]?.signal.aborted).toBe(true);
  expect(h.uploadCalls[0]?.signal.aborted).toBe(false);
  await waitFor(() => expect(h.input()).toHaveValue("second"));
  expect(within(h.form()).getByText("two.txt")).toBeVisible();
  expect(screen.queryByText(/Upload failed/)).toBeNull();

  await act(async () => {
    h.uploadCalls[0]?.result.resolve(uploadDescriptor("one.txt"));
    h.uploadCalls[1]?.result.resolve(uploadDescriptor("two.txt"));
  });
  await waitFor(() => expect(h.publish).toHaveBeenCalledTimes(1));
  expect(h.publish.mock.calls[0]?.[0].content).toContain("first");
  expect(h.publish.mock.calls[0]?.[0].content).toContain("[one.txt](<");
  expect(h.publish.mock.calls[0]?.[0].tags).not.toContainEqual([
    "e",
    root,
    "",
    "root",
  ]);
  expect(screen.queryByText(/^Uploading/)).toBeNull();
});

it.each([
  ["an unrelated channel's access is revoked", "revoke"],
  ["the cache is cleared", "clear"],
] as const)("restores a background send when %s", async (_case, trigger) => {
  const h = await mountUploadComposer();
  if (trigger === "revoke") await h.otherMembership(true);
  attachByPaste(h.input(), attachmentFile());
  await userEvent.type(h.input(), "caption");
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  expect(h.input()).toHaveValue("");

  if (trigger === "revoke") await h.otherMembership(false);
  else await act(() => h.owner.clearCache());
  expect(h.uploadCalls[0]?.signal.aborted).toBe(true);
  await act(async () => {
    h.uploadCalls[0]?.result.resolve(uploadDescriptor());
  });
  await waitFor(() => expect(h.input()).toHaveValue("caption"));
  expect(within(h.form()).getByText("notes.txt")).toBeVisible();
  expect(screen.getByText(/^Upload failed: /)).toBeVisible();
  expect(screen.queryByText(/^Uploading/)).toBeNull();
  expect(h.publish).not.toHaveBeenCalled();
});

it("uses transferred sizes after image preparation and resets unknown totals on retry", async () => {
  vi.stubGlobal(
    "URL",
    Object.assign(URL, {
      createObjectURL: vi.fn(() => "blob:photo"),
      revokeObjectURL: vi.fn(),
    }),
  );
  const h = await mountUploadComposer();
  // A GIF comment is metadata: preparation strips it before transfer.
  const gif = new File(
    [
      new Uint8Array([
        ...new TextEncoder().encode("GIF89a"),
        1,
        0,
        1,
        0,
        0,
        0,
        0,
        0x21,
        0xfe,
        4,
        1,
        2,
        3,
        4,
        0,
        0x3b,
      ]),
    ],
    "photo.gif",
    { type: "image/gif" },
  );
  attachByPaste(h.input(), gif);
  attachByPaste(h.input(), attachmentFile("note.txt", "1234567890"));
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  const first = h.uploadCalls[0];
  expect(first?.file.size).toBeLessThan(gif.size);
  const bar = screen.getByRole("progressbar", { name: "Uploading" });
  act(() => first?.progress?.(first.file.size, first.file.size));
  // The next file has not been prepared: its transfer size is still unknown.
  expect(bar).not.toHaveAttribute("aria-valuenow");
  await act(async () =>
    first?.result.resolve({
      ...uploadDescriptor("photo.gif"),
      size: first.file.size,
    }),
  );
  await waitFor(() => expect(h.uploadCalls).toHaveLength(2));
  act(() => h.uploadCalls[1]?.progress?.(5, 10));
  const expected = Math.round(
    (((first?.file.size ?? 0) + 5) / ((first?.file.size ?? 0) + 10)) * 100,
  );
  expect(bar).toHaveAttribute("aria-valuenow", String(expected));
  expect(bar.firstElementChild).toHaveStyle({
    transform: `scaleX(${((first?.file.size ?? 0) + 5) / ((first?.file.size ?? 0) + 10)})`,
  });
  expect(h.publish).not.toHaveBeenCalled();
  await act(async () =>
    h.uploadCalls[1]?.result.reject(new Error("relay down")),
  );
  await waitFor(() => expect(screen.queryByRole("progressbar")).toBeNull());
  await userEvent.click(screen.getByRole("button", { name: /Retry/ }));
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(3));
  const retryBar = screen.getByRole("progressbar", { name: "Uploading" });
  expect(retryBar).not.toHaveAttribute("aria-valuenow");
  act(() => h.uploadCalls[2]?.progress?.(2, 20));
  expect(retryBar).toHaveAttribute(
    "aria-valuenow",
    String(
      Math.round(
        (((first?.file.size ?? 0) + 2) / ((first?.file.size ?? 0) + 20)) * 100,
      ),
    ),
  );
  await act(async () =>
    h.uploadCalls[2]?.result.resolve({
      ...uploadDescriptor("note.txt"),
      size: 10,
    }),
  );
  await waitFor(() => expect(h.publish).toHaveBeenCalledTimes(1));
});

it("fills the bar for a single-chunk upload while its response is held", async () => {
  const h = await mountUploadComposer();
  attachByPaste(h.input(), attachmentFile());
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  const bar = screen.getByRole("progressbar", { name: "Uploading" });
  expect(bar).not.toHaveAttribute("aria-valuenow");
  // The host's only report for a body within one chunk is the whole body.
  act(() => h.uploadCalls[0]?.progress?.(5, 5));
  expect(bar).toHaveAttribute("aria-valuenow", "100");
  expect(screen.getByText("Uploading", { exact: true })).toBeVisible();
  expect(h.publish).not.toHaveBeenCalled();
  await act(async () => {
    h.uploadCalls[0]?.result.resolve(uploadDescriptor());
  });
  await waitFor(() => expect(h.publish).toHaveBeenCalledTimes(1));
  expect(screen.queryByRole("progressbar")).toBeNull();
});

it("never publishes a background send after its session closes", async () => {
  const h = await mountUploadComposer();
  attachByPaste(h.input(), attachmentFile());
  await userEvent.type(h.input(), "caption");
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  expect(screen.getByText("Uploading", { exact: true })).toBeVisible();

  act(() => h.owner.dispose());
  expect(h.uploadCalls[0]?.signal.aborted).toBe(true);
  await act(async () => {
    h.uploadCalls[0]?.result.resolve(uploadDescriptor());
  });
  await waitFor(() => expect(screen.queryByText(/^Uploading/)).toBeNull());
  expect(h.publish).not.toHaveBeenCalled();
  expect(h.sign).not.toHaveBeenCalled();
});

it("retries publish-unknown attachment sends with the same signed event and no re-upload", async () => {
  let attempts = 0;
  const h = await mountUploadComposer({
    publish: async () => {
      attempts++;
      if (attempts === 1) throw new Error("lost acknowledgement");
    },
  });
  attachByPaste(h.input(), attachmentFile());
  fireEvent.click(h.send());
  await waitFor(() => expect(h.uploadCalls).toHaveLength(1));
  await act(async () => {
    h.uploadCalls[0]?.result.resolve(uploadDescriptor());
  });
  await waitFor(() =>
    expect(h.owner.session.outbox?.snapshot()[0]?.delivery).toBe("unknown"),
  );
  const firstEvent = h.publish.mock.calls[0]?.[0];
  if (!firstEvent) throw new Error("Missing first publish");
  h.owner.session.messages.retry(firstEvent.id);
  await waitFor(() => expect(h.publish).toHaveBeenCalledTimes(2));
  expect(h.publish.mock.calls[1]?.[0]).toEqual(firstEvent);
  expect(h.sign).toHaveBeenCalledTimes(1);
  expect(h.uploadCalls).toHaveLength(1);
});

it("sends channel messages and thread replies through real form and keyboard events", async () => {
  const h = mount();
  await h.user.type(h.input(), "channel draft");
  await h.user.click(screen.getByRole("button", { name: "Send message" }));
  expect(h.messages.send).toHaveBeenCalledExactlyOnceWith(
    "channel",
    "channel draft",
    [],
    [],
  );
  expect(h.messages.reply).not.toHaveBeenCalled();
  h.retarget({ threadRootId: "root" });
  await h.user.type(h.input(), "thread draft");
  await h.user.keyboard("{Shift>}{Enter}{/Shift}");
  expect(h.input()).toHaveValue("thread draft\n");
  // This checks the composition guard, not native IME behavior.
  fireEvent.keyDown(h.input(), { key: "Enter", isComposing: true });
  expect(h.messages.reply).not.toHaveBeenCalled();
  await h.user.keyboard("{Enter}");
  expect(h.messages.reply).toHaveBeenCalledExactlyOnceWith(
    "channel",
    "root",
    "thread draft\n",
    [],
    [],
  );
  expect(h.messages.send).toHaveBeenCalledTimes(1);
  expect(h.onSend.mock.calls).toEqual([["channel-id"], ["reply-id"]]);
  expect(h.input()).toHaveValue("");
});

it("opens a code block as ``` is typed without waiting for Enter, then sends the fenced block", async () => {
  const h = mount();
  await h.user.type(h.input(), "```");
  expect(h.input().querySelector("pre > code")).not.toBeNull();
  expect(h.input()).toHaveValue("");
  expect(h.messages.send).not.toHaveBeenCalled();
  await h.user.keyboard("const answer = 42;{Shift>}{Enter}{/Shift}answer");
  expect(h.input()).toHaveValue("const answer = 42;\nanswer");
  await h.user.keyboard("{Enter}");
  expect(h.messages.send).toHaveBeenCalledExactlyOnceWith(
    "channel",
    "```\nconst answer = 42;\nanswer\n```",
    [],
    [],
  );
  expect(h.input()).toHaveValue("");
  expect(h.input().querySelector("pre")).toBeNull();
});

it.each([
  ["- ", "ul", "- first\n- second"],
  ["3. ", "ol", "3. first\n4. second"],
])(
  "continues a typed %s list on Enter and sends with the button",
  async (marker, tag, markdown) => {
    const h = mount();
    await h.user.type(h.input(), `${marker}first`);
    expect(h.input().querySelector(`${tag} > li`)).toHaveTextContent("first");
    // Composition confirmation must neither split nor send the list.
    fireEvent.keyDown(h.input(), { key: "Enter", isComposing: true });
    fireEvent.keyDown(h.input(), { key: "Enter", keyCode: 229 });
    expect(h.input().querySelectorAll(`${tag} > li`)).toHaveLength(1);
    await h.user.keyboard("{Enter}second");
    expect(h.input().querySelectorAll(`${tag} > li`)).toHaveLength(2);
    expect(h.messages.send).not.toHaveBeenCalled();
    await h.user.click(screen.getByRole("button", { name: "Send message" }));
    expect(h.messages.send).toHaveBeenCalledExactlyOnceWith(
      "channel",
      markdown,
      [],
      [],
    );
    expect(h.input()).toHaveValue("");
  },
);

it("exits an empty list item on Enter, then sends from ordinary prose", async () => {
  const h = mount();
  await h.user.type(h.input(), "- first");
  await h.user.keyboard("{Shift>}{Enter}{/Shift}second{Enter}{Enter}");
  expect(h.input().querySelectorAll("ul > li")).toHaveLength(2);
  expect(h.input().querySelector(":scope > p")).not.toBeNull();
  fireEvent.keyDown(h.input(), { key: "Enter", repeat: true });
  expect(h.messages.send).not.toHaveBeenCalled();
  await h.user.keyboard("outside{Enter}");
  expect(h.messages.send).toHaveBeenCalledExactlyOnceWith(
    "channel",
    "- first\n- second\n\noutside",
    [],
    [],
  );
});

it("uses Enter to leave a nested empty item without sending", async () => {
  const h = mount();
  await h.user.type(h.input(), "- first{Enter}nested{Tab}{Enter}{Enter}");
  expect(h.input().querySelector("ul ul > li")).toHaveTextContent("nested");
  expect(h.input().querySelectorAll(":scope > ul > li")).toHaveLength(2);
  expect(h.messages.send).not.toHaveBeenCalled();
  await h.user.keyboard("{Enter}");
  expect(h.input().querySelector(":scope > p")).not.toBeNull();
  expect(h.messages.send).not.toHaveBeenCalled();
});

it("accepts a completion before handling list Enter", async () => {
  const h = mount();
  await h.user.type(h.input(), "- !search");
  expect(h.publish(h.completionRequests.length - 1)).not.toBe(false);
  await h.user.keyboard("{Enter}");
  expect(h.input().querySelectorAll("ul > li")).toHaveLength(1);
  expect(h.input()).toHaveValue("chosen ");
  expect(h.messages.send).not.toHaveBeenCalled();
});

it("sends a pasted fenced block verbatim on Enter instead of opening a block from its closing fence", async () => {
  const h = mount();
  act(() => {
    h.input().focus();
    fireEvent.paste(h.input(), {
      clipboardData: {
        items: [],
        getData: (type: string) =>
          type === "text/plain" ? "```\ncode\n```" : "",
      },
    });
  });
  expect(h.input()).toHaveValue("```\ncode\n```");
  await h.user.keyboard("{Enter}");
  expect(h.input().querySelector("pre")).toBeNull();
  expect(h.messages.send).toHaveBeenCalledExactlyOnceWith(
    "channel",
    "```\ncode\n```",
    [],
    [],
  );
  expect(h.input()).toHaveValue("");
});

it("keeps a composed message unchanged through caret keys at its end and refuses a Right Arrow committed as text in either form", async () => {
  const h = mount();
  await h.user.type(h.input(), "```");
  expect(h.input().querySelector("pre")).not.toBeNull();
  await h.user.keyboard(
    "Hello!{Shift>}{Enter}{Enter}{/Shift}acascac{Shift>}{Enter}{/Shift}a**a** _a_{Shift>}{Enter}{/Shift}- acacs{Shift>}{Enter}{Enter}{/Shift}",
  );
  expect(h.input().querySelector("pre code")).toHaveTextContent("Hello!");
  expect(h.input().querySelector("strong")).toHaveTextContent("a");
  expect(h.input().querySelector("em")).toHaveTextContent("a");
  expect(h.input().querySelector("ul > li")).toHaveTextContent("acacs");
  expect(h.input()).toHaveValue("Hello!\nacascac\naa a\nacacs\n");
  const html = h.input().innerHTML;
  for (let i = 0; i < 11; i++) await h.user.keyboard("{ArrowRight}");
  for (const key of [
    "ArrowLeft",
    "ArrowUp",
    "ArrowDown",
    "Shift",
    "Meta",
    "Escape",
  ])
    await h.user.keyboard(`{${key}}`);
  // jsdom does not model Home and End on a contenteditable element.
  for (const key of ["Home", "End"]) {
    fireEvent.keyDown(h.input(), { key, code: key });
    fireEvent.keyUp(h.input(), { key, code: key });
  }
  expect(h.input()).toHaveValue("Hello!\nacascac\naa a\nacacs\n");
  expect(h.input().innerHTML).toBe(html);
  expect(h.messages.send).not.toHaveBeenCalled();
  // The desktop build committed Right Arrow's raw keyboard-layout translation
  // U+001D; AppKit's function-key character for the key is U+F703. Neither
  // has a glyph, so each assertion names its form rather than the character.
  for (const [label, character] of [
    ["Right Arrow's layout translation U+001D", "\u001D"],
    ["Right Arrow's function-key character U+F703", "\uF703"],
  ] as const) {
    let prevented = false;
    act(() => {
      h.input().focus();
      prevented = !h.input().dispatchEvent(
        new InputEvent("beforeinput", {
          bubbles: true,
          cancelable: true,
          inputType: "insertText",
          data: character,
        }),
      );
    });
    expect(prevented, label).toBe(true);
    expect(h.input(), label).toHaveValue("Hello!\nacascac\naa a\nacacs\n");
    expect(h.input().innerHTML, label).toBe(html);
    expect(h.messages.send, label).not.toHaveBeenCalled();
  }
});

it("prefixes thread replies with the selected media time and clears it after send", async () => {
  const clearMediaTime = vi.fn();
  const h = mount({
    threadRootId: "root",
    mediaTimeSeconds: 72.8,
    clearMediaTime,
  });
  expect(screen.getByText("Commenting at 1:12")).toBeVisible();
  await h.user.type(h.input(), "trim this ");
  await h.user.click(screen.getByRole("button", { name: "Send message" }));
  expect(h.messages.reply).toHaveBeenCalledExactlyOnceWith(
    "channel",
    "root",
    "⏱ 1:12 — trim this",
    [],
    [],
  );
  expect(clearMediaTime).toHaveBeenCalledOnce();
  expect(h.input()).toHaveValue("");
});

it("lets the visible media time indicator dismiss without sending", async () => {
  const clearMediaTime = vi.fn();
  const h = mount({
    threadRootId: "root",
    mediaTimeSeconds: 12,
    clearMediaTime,
  });
  await h.user.click(screen.getByRole("button", { name: "Remove video time" }));
  expect(clearMediaTime).toHaveBeenCalledOnce();
  expect(h.messages.reply).not.toHaveBeenCalled();
});

it("hides the media time indicator while keeping the send prefix", async () => {
  const clearMediaTime = vi.fn();
  const h = mount({
    threadRootId: "root",
    mediaTimeSeconds: 12,
    clearMediaTime,
    hideMediaTimeIndicator: true,
  });
  expect(screen.queryByText("Commenting at 0:12")).not.toBeInTheDocument();
  expect(
    screen.queryByRole("button", { name: "Remove video time" }),
  ).not.toBeInTheDocument();
  await h.user.type(h.input(), "hidden frame");
  await h.user.click(screen.getByRole("button", { name: "Send message" }));
  expect(h.messages.reply).toHaveBeenCalledExactlyOnceWith(
    "channel",
    "root",
    "⏱ 0:12 — hidden frame",
    [],
    [],
  );
  expect(clearMediaTime).toHaveBeenCalledOnce();
});

it("isolates channel, thread and identity drafts through retargeting and remounting", () => {
  const h = mount();
  h.fill("channel draft");
  h.retarget({ threadRootId: "one" });
  expect(h.input()).toHaveValue("");
  h.fill("first thread");
  h.retarget({ threadRootId: "two" });
  h.fill("second thread");
  h.retarget({ threadRootId: "one", scope: "other identity" });
  expect(h.input()).toHaveValue("");
  h.fill("other identity");
  h.retarget({ threadRootId: "one", scope: "scope" });
  expect(h.input()).toHaveValue("first thread");
  h.retarget({ threadRootId: "two" });
  expect(h.input()).toHaveValue("second thread");
  h.unmount();
  const restored = mount();
  expect(restored.input()).toHaveValue("channel draft");
  restored.retarget({ threadRootId: "one", scope: "other identity" });
  expect(restored.input()).toHaveValue("other identity");
});

it("retains rejected intent and clears the draft only after the outbox accepts it", async () => {
  const h = mount({ threadRootId: "root" });
  h.fill("retry me");
  h.messages.reply.mockImplementationOnce(() => {
    throw new Error("outbox full");
  });
  await h.user.click(screen.getByRole("button", { name: "Send message" }));
  expect(h.input()).toHaveValue("retry me");
  expect(h.onSend).not.toHaveBeenCalled();
  expect(screen.getByRole("alert")).toHaveTextContent("outbox full");
  await h.user.click(screen.getByRole("button", { name: "Send message" }));
  expect(h.messages.reply).toHaveBeenCalledTimes(2);
  expect(h.input()).toHaveValue("");
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
});

it("labels simultaneous composers independently and prevents disabled or unsupported writes", async () => {
  const channel = mount();
  const thread = mount({ threadRootId: "root" });
  expect(screen.getByLabelText("Message #General")).toBe(channel.input());
  expect(screen.getByRole("textbox", { name: "Reply to thread" })).toBe(
    thread.input(),
  );
  expect(thread.input().id).not.toBe(channel.input().id);
  thread.fill("retain me");
  thread.retarget({ disabled: true });
  expect(thread.input()).toHaveAttribute("aria-disabled", "true");
  expect(thread.input()).toHaveAttribute("contenteditable", "false");
  await thread.user.click(
    within(thread.container).getByRole("button", { name: "Send message" }),
  );
  expect(thread.messages.reply).not.toHaveBeenCalled();
  expect(thread.input()).toHaveValue("retain me");
  thread.retarget({
    session: {
      ...thread.session,
      outbox: { supports: () => false },
    } as unknown as RelaySession,
  });
  expect(
    within(thread.container).queryByRole("textbox"),
  ).not.toBeInTheDocument();
  expect(
    within(thread.container).getByText(
      "This relay connection supports reading only.",
    ),
  ).toBeVisible();
});

it("subscribes to emoji changes and releases the subscription when unmounted", () => {
  const h = mount();
  h.fill(":party:");
  expect(h.input().querySelectorAll("img[alt=':party:']")).toHaveLength(0);
  expect(h.emojiListeners.size).toBe(1);
  expect(h.session.emoji.ensure).toHaveBeenCalled();
  h.setEmoji([{ shortcode: "party", url: "https://emoji.test/party.png" }]);
  expect(h.input().querySelectorAll("img[alt=':party:']")).toHaveLength(1);
  h.setEmoji([]);
  expect(h.input().querySelectorAll("img[alt=':party:']")).toHaveLength(0);
  h.unmount();
  expect(h.emojiListeners.size).toBe(0);
});

it("enlarges Unicode-only drafts and restores normal text presentation", () => {
  const h = mount();
  for (const draft of ["😀", "😀 🙏 👏", "😀 🙏 👏 😄"]) {
    h.fill(draft);
    expect(h.input()).toHaveAttribute("data-single-emoji", "true");
    h.fill(`${draft} hello`);
    expect(h.input()).not.toHaveAttribute("data-single-emoji");
  }
});

it.each([undefined, "root"])(
  "persists exact namesake recipients for %s without resolving typed prose",
  async (root) => {
    const options = root ? { threadRootId: root } : {};
    let h = mount(options);
    h.fill("@Honey prose only");
    h.submit();
    expect(
      (root ? h.messages.reply : h.messages.send).mock.calls[0]?.[root ? 3 : 2],
    ).toEqual([]);
    h.fill("Please help ");
    await h.user.click(screen.getByRole("button", { name: "First Honey" }));
    await h.user.click(screen.getByRole("button", { name: "Second Honey" }));
    h.unmount();
    h = mount(options);
    expect(h.input()).toHaveValue("Please help @Honey @Honey ");
    expect(
      within(h.input()).getAllByRole("img", {
        name: /^Person Honey, public key ending/,
      }),
    ).toHaveLength(2);
    expect(
      within(
        screen.getByRole("region", { name: "Explicit mentions" }),
      ).getAllByRole("button"),
    ).toHaveLength(2);
    h.submit();
    expect(
      (root ? h.messages.reply : h.messages.send).mock.calls[0]?.[root ? 3 : 2],
    ).toEqual([first.pubkey, second.pubkey]);
    expect(h.input()).toHaveValue("");
    h.unmount();
    h = mount(options);
    h.fill("@Honey typed after send");
    h.submit();
    expect(
      (root ? h.messages.reply : h.messages.send).mock.calls[0]?.[root ? 3 : 2],
    ).toEqual([]);
  },
);

it("restores live profile avatars with one removal control per exact recipient", async () => {
  const h = mount();
  const media = vi
    .spyOn(h.session, "media")
    .mockImplementation((url) =>
      url ? `https://media.test/${url}` : undefined,
    );
  act(() => {
    h.commands().insertMention(first);
    h.commands().insertMention(second);
    h.commands().insertMention(second);
  });
  let region = screen.getByRole("region", {
    name: "Explicit mentions",
  });
  const controls = within(region).getAllByRole("button");
  expect(controls).toHaveLength(2);
  expect(controls[1]).toHaveTextContent("H");
  // Profiles can arrive after draft restoration; artwork must update without an edit.
  h.setProfiles(
    new Map([
      [first.pubkey, { name: "Honey", picture: "person.png" }],
      [second.pubkey, { name: "Honey", picture: "agent.png", isAgent: true }],
    ]),
  );
  expect(controls[0]?.querySelector(".buzz-avatar")).toHaveAttribute(
    "data-avatar-shape",
    "circle",
  );
  expect(controls[1]?.querySelector(".buzz-avatar")).toHaveAttribute(
    "data-avatar-shape",
    "squircle",
  );
  expect(controls[1]?.querySelector("img")).toHaveAttribute(
    "src",
    "https://media.test/agent.png",
  );
  expect(media).toHaveBeenCalledWith("agent.png", "small");
  // Loaded-library hints update both artwork layers without another keystroke;
  // clearing them removes only that fallback, not self-declared agent metadata.
  for (const control of controls)
    expect(control.querySelectorAll("[data-avatar-shape]")).toHaveLength(2);
  for (const identities of [[first], []]) {
    h.setLibrary(identities);
    for (const [index, control] of controls.entries())
      for (const artwork of control.querySelectorAll("[data-avatar-shape]"))
        expect(artwork).toHaveAttribute(
          "data-avatar-shape",
          index === 1 || identities.length ? "squircle" : "circle",
        );
  }
  expect(h.session.agentLibrary.refresh).not.toHaveBeenCalled();
  h.retarget({ disabled: true });
  for (const control of controls) expect(control).toBeDisabled();
  h.retarget({ disabled: false, extensions: undefined });
  // The optional picker does not own saved intent or its removal controls.
  region = screen.getByRole("region", { name: "Explicit mentions" });
  await h.user.click(
    within(region).getByRole("button", {
      name: `Remove mention Honey ${second.pubkey}`,
    }),
  );
  expect(within(region).getAllByRole("button")).toHaveLength(1);
  expect(h.input()).toHaveValue("@Honey @Honey @Honey ");
  expect(h.input().querySelectorAll(".inline-chip")).toHaveLength(1);
  h.submit();
  expect(h.messages.send).toHaveBeenCalledWith(
    "channel",
    "@Honey @Honey @Honey ",
    [first.pubkey],
    [],
  );
  expect(
    screen.queryByRole("region", { name: "Explicit mentions" }),
  ).not.toBeInTheDocument();
});

// Explicit notification intent must remain visible even where Markdown previews are suppressed.
it.each([
  ["inline code", "`", " `"],
  ["fenced code", "```\n", "\n```"],
  ["indented code", "    ", ""],
  ["image", "![", "](https://example.test/image.png)"],
  [
    "image reference",
    "![",
    "][image]\n\n[image]: https://example.test/image.png",
  ],
  ["definition", '[image]: https://example.test/image.png "', '"'],
  ["HTML", "<!-- ", " -->"],
  ["link label", "[", "](https://example.test)"],
  ["deep Markdown", "> ".repeat(101), ""],
])(
  "discloses selected namesakes in %s before and after restoring a draft",
  (_kind, prefix, suffix) => {
    let h = mount();
    h.fill(`${prefix}@Honey ${suffix}`);
    expect(h.input().querySelector(".inline-chip")).toBeNull();
    h.submit();
    expect(h.messages.send.mock.calls.at(-1)?.[2]).toEqual([]);
    h.fill(`${prefix}${suffix}`);
    h.input().setSelectionRange(prefix.length, prefix.length);
    act(() => {
      h.commands().insertMention(first);
      h.commands().insertMention(second);
    });
    const text = `${prefix}@Honey @Honey ${suffix}`;
    const labels = [
      "Person Honey, public key ending r c a j",
      "Person Honey, public key ending 0 4 h u",
    ];
    const check = () => {
      expect(h.input()).toHaveValue(text);
      for (const name of labels)
        expect(within(h.input()).getByRole("img", { name })).toBeVisible();
    };
    check();
    h.unmount();
    h = mount();
    check();
    h.submit();
    expect(h.messages.send).toHaveBeenCalledWith(
      "channel",
      text,
      [first.pubkey, second.pubkey],
      [],
    );
  },
);

it.each([undefined])(
  "keeps an untouched mention when smart punctuation replaces text behind the caret in %s",
  async (root) => {
    const h = mount(root ? { threadRootId: root } : {});
    act(() => {
      h.commands().insertMention(first);
      h.commands().insertText("can you see this is's");
    });
    const input = h.input();
    const paragraph = input.querySelector("p");
    if (!paragraph) throw new Error("Missing editor paragraph");
    const text = [...paragraph.childNodes].find(
      (node) => node instanceof Text && node.data.includes("is's"),
    );
    if (!(text instanceof Text)) throw new Error("Missing editable text");
    const quote = text.data.indexOf("'");
    expect(quote).toBeGreaterThan(0);
    const target = document.createRange();
    target.setStart(text, quote);
    target.setEnd(text, quote + 1);
    // WebKit's replacement range is behind the caret, not the selection.
    act(() => input.setSelectionRange(input.value.length, input.value.length));
    const before = new InputEvent("beforeinput", {
      bubbles: true,
      inputType: "insertReplacementText",
      data: "’",
    });
    Object.defineProperty(before, "getTargetRanges", {
      value: () => [target],
    });
    fireEvent(input, before);
    text.replaceData(quote, 1, "’");
    fireEvent.input(input, {
      inputType: "insertReplacementText",
      data: "’",
    });
    await waitFor(() =>
      expect(input).toHaveValue("@Honey can you see this is’s"),
    );
    expect(
      within(input).getByRole("img", { name: "Person Honey" }),
    ).toBeVisible();
    h.submit();
    expect(
      (root ? h.messages.reply : h.messages.send).mock.calls[0]?.[root ? 3 : 2],
    ).toEqual([first.pubkey]);
  },
);

it("qualifies both namesakes retroactively without changing source and removes qualifiers with ambiguity", () => {
  const h = mount();
  act(() => {
    h.commands().insertMention(first);
  });
  expect(
    within(h.input()).getByRole("img", { name: "Person Honey" }),
  ).toBeVisible();
  act(() => {
    h.commands().insertMention(first);
  });
  expect(h.input().textContent).not.toContain("npub");
  act(() => {
    h.commands().insertMention(second);
  });
  expect(h.input()).toHaveValue("@Honey @Honey @Honey ");
  expect(
    within(h.input()).getAllByRole("img", {
      name: "Person Honey, public key ending r c a j",
    }),
  ).toHaveLength(2);
  expect(
    within(h.input()).getByRole("img", {
      name: "Person Honey, public key ending 0 4 h u",
    }),
  ).toHaveTextContent("Honey · 04hu");
  h.input().setSelectionRange(14, 20);
  act(() => {
    h.commands().insertText("");
  });
  expect(h.input()).toHaveValue("@Honey @Honey  ");
  expect(h.input().textContent).not.toContain("npub");
  h.submit();
  expect(h.messages.send).toHaveBeenCalledWith(
    "channel",
    "@Honey @Honey  ",
    [first.pubkey, first.pubkey],
    [],
  );
});

it.each([0, 7])(
  "does not replay qualifier motion after removing at %i or restoring a destination draft",
  (start) => {
    const h = mount();
    act(() => {
      h.commands().insertMention(first);
    });
    act(() => {
      h.commands().insertMention(second);
    });
    expect(h.input().querySelectorAll("[data-reveal]")).toHaveLength(1);
    h.input().setSelectionRange(start, start + 6);
    act(() => {
      h.commands().insertText("");
    });
    act(() => {
      h.commands().insertMention(start === 0 ? first : second);
    });
    expect(h.input().querySelectorAll("[data-reveal]")).toHaveLength(0);
    h.retarget({ channelId: "other" });
    act(() => {
      h.commands().insertMention(first);
    });
    h.retarget({ channelId: "channel" });
    expect(h.input().querySelectorAll(".inline-chip-qualifier")).toHaveLength(
      2,
    );
    expect(h.input().querySelectorAll("[data-reveal]")).toHaveLength(0);
  },
);

it("replacing an inline mention with ordinary prose removes notification intent", async () => {
  const h = mount();
  await h.user.click(screen.getByRole("button", { name: "First Honey" }));
  expect(
    screen.getByRole("region", { name: "Explicit mentions" }),
  ).toBeVisible();
  h.fill("no recipient now");
  h.submit();
  expect(h.messages.send.mock.calls[0]?.[2]).toEqual([]);
  await h.user.click(screen.getByRole("button", { name: "First Honey" }));
  expect(
    within(h.input()).getByRole("img", { name: "Person Honey" }),
  ).toBeVisible();
  h.input().setSelectionRange(0, 6);
  act(() => {
    h.commands().insertText("Honey");
  });
  expect(within(h.input()).queryByRole("img")).not.toBeInTheDocument();
  h.submit();
  expect(h.messages.send.mock.calls[1]?.[2]).toEqual([]);
});

it("ambiguous namesake replacement cannot notify the wrong remaining identity", async () => {
  const h = mount();
  await h.user.click(screen.getByRole("button", { name: "First Honey" }));
  await h.user.click(screen.getByRole("button", { name: "Second Honey" }));
  h.fill("@Honey help");
  h.submit();
  expect(h.messages.send.mock.calls[0]?.[2]).toEqual([]);
});

it("serializes tool commands in one React batch and rejects malformed recipients", () => {
  const h = mount();
  h.fill("Hi ");
  act(() => {
    const { insertText, insertMention } = h.commands();
    expect(insertText("there ")).toBe(true);
    expect(insertMention(first)).toBe(true);
    expect(insertText("and ")).toBe(true);
    expect(insertMention(second)).toBe(true);
    expect(insertMention({ pubkey: "wrong", name: "Honey" })).toBe(false);
    expect(insertMention({ ...first, name: "  " })).toBe(false);
    expect(insertMention(null as unknown as typeof first)).toBe(false);
  });
  h.submit();
  expect(h.messages.send).toHaveBeenCalledExactlyOnceWith(
    "channel",
    "Hi there @Honey and @Honey ",
    [first.pubkey, second.pubkey],
    [],
  );
});

it("revokes captured tool commands after retargeting, disabling and unmounting", () => {
  const h = mount();
  h.fill("channel draft");
  const channel = h.commands();
  h.retarget({ threadRootId: "root" });
  act(() => {
    expect(channel.insertText("stale")).toBe(false);
    expect(channel.insertMention(first)).toBe(false);
    expect(channel.insertResource(resource)).not.toBe(true);
  });
  expect(h.input()).toHaveValue("");
  const thread = h.commands();
  h.retarget({ disabled: true });
  act(() => {
    expect(thread.insertText("disabled")).toBe(false);
    expect(thread.insertResource(resource)).not.toBe(true);
  });
  h.retarget({ disabled: false });
  act(() => {
    expect(h.commands().insertText("current")).toBe(true);
  });
  expect(h.input()).toHaveValue("current");
  const current = h.commands();
  h.unmount();
  act(() => {
    expect(current.insertText("unmounted")).toBe(false);
  });
});

it("keeps custom emoji text readable and sends repeated shortcodes unchanged", () => {
  const h = mount();
  h.setEmoji([{ shortcode: "party", url: "https://emoji.test/party.png" }]);
  act(() => {
    expect(h.commands().insertText(":party:")).toBe(true);
  });
  expect(h.input()).toHaveValue(":party:");
  expect(h.input()).toHaveAttribute("data-single-emoji", "true");
  act(() => {
    expect(h.commands().insertText(":party:")).toBe(true);
  });
  expect(h.input()).toHaveValue(":party::party:");
  expect(h.input().querySelectorAll("img[alt=':party:']")).toHaveLength(2);
  h.submit();
  expect(h.messages.send).toHaveBeenCalledExactlyOnceWith(
    "channel",
    ":party::party:",
    [],
    [],
  );
});

it("renders a leading custom emoji inline without changing trailing text", () => {
  const h = mount();
  h.setEmoji([{ shortcode: "bufo", url: "https://emoji.test/bufo.png" }]);
  h.fill(":bufo:lakjsdlkjflakjsdf");
  expect(h.input()).not.toHaveAttribute("data-single-emoji");
  expect(h.input()).toHaveValue(":bufo:lakjsdlkjflakjsdf");
  expect(h.container.querySelectorAll("img")).toHaveLength(1);
  h.submit();
  expect(h.messages.send).toHaveBeenCalledExactlyOnceWith(
    "channel",
    ":bufo:lakjsdlkjflakjsdf",
    [],
    [],
  );
});

/** The real Emoji provider; the fixture provider above never publishes an exact match. */
function mountEmojiTypeahead() {
  // jsdom has no canvas; previews fall back to the uncentred glyph.
  const context = vi
    .spyOn(HTMLCanvasElement.prototype, "getContext")
    .mockImplementation(() => null);
  onTestFinished(() => context.mockRestore());
  const empty: [] = [];
  const none = { snapshot: () => empty, subscribe: () => () => {} };
  const completions: readonly Contribution<ComposerCompletion>[] = [
    {
      id: "typeahead",
      key: "buzz.emoji/typeahead",
      pluginId: "buzz.emoji",
      revision: "1",
      title: "Emoji",
      match: ({ text, start }) => emojiQuery(text, start),
      component: EmojiCompletion,
    },
  ];
  return mount({
    extensions: {
      tools: none,
      inline: none,
      completions: { snapshot: () => completions, subscribe: () => () => {} },
    },
  });
}
// ":" is Shift+; on US layouts, so the typed colon arrives with shiftKey set.
const colon = (input: ComposerInputElement) =>
  fireEvent.keyDown(input, { key: ":", shiftKey: true });

it.each([
  ["hello :-1", "hello 👎"],
  [":+1", "👍"],
  ["say :SMILE", "say 😄"],
])(
  "replaces an exact shortcode with its emoji on the closing colon: %s",
  async (typed, expected) => {
    const h = mountEmojiTypeahead();
    h.fill(typed);
    const label = `:${typed.slice(typed.lastIndexOf(":") + 1).toLowerCase()}:`;
    await screen.findByRole("option", { name: label });
    expect(colon(h.input())).toBe(false);
    expect(h.input()).toHaveValue(expected);
    expect([h.input().selectionStart, h.input().selectionEnd]).toEqual([
      expected.length,
      expected.length,
    ]);
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
  },
);

it("keeps the typed colon after a partial shortcode and never accepts emoji on Space", async () => {
  const h = mountEmojiTypeahead();
  h.fill("hello :smil");
  await screen.findByRole("option", { name: ":smile:" });
  expect(colon(h.input())).toBe(true);
  expect(h.input()).toHaveValue("hello :smil");
  h.fill("hello :-1");
  await screen.findByRole("option", { name: ":-1:" });
  expect(fireEvent.keyDown(h.input(), { key: " " })).toBe(true);
  expect(h.input()).toHaveValue("hello :-1");
});

it.each(["at 10:30"])(
  "leaves the colon in times and URLs alone: %s",
  (typed) => {
    const h = mountEmojiTypeahead();
    h.fill(typed);
    // No syntax match means no provider mounts, so no asynchronous search is pending.
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    expect(colon(h.input())).toBe(true);
    expect(h.input()).toHaveValue(typed);
  },
);

it("keeps a shortcode literal inside inline code", async () => {
  const h = mountEmojiTypeahead();
  h.fill("run :-1");
  act(() => {
    h.input().setSelectionRange(0, 7);
    h.input().toggleFormat("code");
    h.input().setSelectionRange(7, 7);
  });
  fireEvent(document, new Event("selectionchange"));
  await screen.findByRole("option", { name: ":-1:" });
  expect(colon(h.input())).toBe(true);
  expect(h.input()).toHaveValue("run :-1");
});

it("waits for the community catalog before authorizing a Unicode replacement", async () => {
  const h = mountEmojiTypeahead();
  h.setEmoji([], "loading");
  h.fill(":smile");
  await screen.findByRole("option", { name: ":smile:" });
  expect(colon(h.input())).toBe(true);
  expect(h.input()).toHaveValue(":smile");
  // Release the pending catalog with a namesake after Unicode has settled.
  h.setEmoji([{ shortcode: "smile", url: "https://emoji.test/smile.png" }]);
  await screen.findByRole("option", { name: ":smile: Unicode emoji" });
  expect(colon(h.input())).toBe(true);
  // A later complete, unambiguous catalog permits the same query without typing.
  h.setEmoji([]);
  await screen.findByRole("option", { name: ":smile:" });
  expect(colon(h.input())).toBe(false);
  expect(h.input()).toHaveValue("😄");
});

it("keeps Unicode suggestions and retry without replacement after a community failure", async () => {
  const h = mountEmojiTypeahead();
  h.setEmoji([], "error");
  h.fill(":smile");
  await screen.findByText(
    "Community emoji unavailable; Unicode results shown.",
  );
  expect(screen.getByRole("option", { name: ":smile:" })).toBeInTheDocument();
  expect(colon(h.input())).toBe(true);
  await h.user.click(screen.getByRole("option", { name: "Retry suggestions" }));
  expect(h.session.emoji.refresh).toHaveBeenCalledOnce();
  h.setEmoji([]);
  await screen.findByRole("option", { name: ":smile:" });
  expect(colon(h.input())).toBe(false);
});

it("keeps custom suggestions and retry without replacement after a Unicode failure", async () => {
  const search = vi
    .spyOn(emojiSearch, "searchEmoji")
    .mockRejectedValue(new Error("chunk unavailable"));
  onTestFinished(() => search.mockRestore());
  const h = mountEmojiTypeahead();
  h.setEmoji([{ shortcode: "party", url: "https://emoji.test/party.png" }]);
  h.fill(":party");
  await screen.findByText("Unicode emoji unavailable. Custom matches shown.");
  expect(screen.getByRole("option", { name: ":party:" })).toBeInTheDocument();
  expect(colon(h.input())).toBe(true);
  search.mockRestore();
  await h.user.click(screen.getByRole("option", { name: "Retry suggestions" }));
  await waitFor(() =>
    expect(
      screen.queryByText("Unicode emoji unavailable. Custom matches shown."),
    ).toBeNull(),
  );
  expect(colon(h.input())).toBe(false);
  expect(h.input()).toHaveValue(":party:");
});

it.each([
  ["```\n:-1\n```", 7],
  ["~~~\n:-1", 7],
  ["`run :-1`", 8],
  ["    :-1", 7],
])(
  "keeps a shortcode literal inside raw Markdown code: %s",
  async (text, caret) => {
    const h = mountEmojiTypeahead();
    h.fill(text);
    act(() => h.input().setSelectionRange(caret, caret));
    fireEvent(document, new Event("selectionchange"));
    await screen.findByRole("option", { name: ":-1:" });
    expect(colon(h.input())).toBe(true);
    expect(h.input()).toHaveValue(text);
  },
);

it("restores the typed colon and collapsed caret with one undo, and isolates subsequent typing", async () => {
  const h = mountEmojiTypeahead();
  h.fill("hello :-1");
  await screen.findByRole("option", { name: ":-1:" });
  expect(colon(h.input())).toBe(false);
  expect(h.input()).toHaveValue("hello 👎");
  act(() => h.input().undo(false));
  expect(h.input()).toHaveValue("hello :-1:");
  expect([h.input().selectionStart, h.input().selectionEnd]).toEqual([10, 10]);
  act(() => h.input().undo(true));
  expect(h.input()).toHaveValue("hello 👎");
  await h.user.type(h.input(), "!");
  act(() => h.input().undo(false));
  expect(h.input()).toHaveValue("hello 👎");
  act(() => h.input().undo(false));
  expect(h.input()).toHaveValue("hello :-1:");
});

it("rejects overlong and over-limit tool edits without changing accepted intent", () => {
  const h = mount();
  h.fill("x".repeat(15999));
  act(() => {
    expect(h.commands().insertMention(first)).toBe(false);
  });
  expect(h.input()).toHaveValue("x".repeat(15999));
  expect(screen.getByRole("alert")).toHaveTextContent("too long");
  h.fill("");
  act(() => {
    for (let i = 0; i < 32; i++)
      expect(h.commands().insertMention(first)).toBe(true);
  });
  const before = h.input().value;
  act(() => {
    expect(h.commands().insertMention(first)).toBe(false);
  });
  expect(h.input()).toHaveValue(before);
  expect(screen.getByRole("alert")).toHaveTextContent("at most 32 recipients");
});

// The production composer must perform enrollment, not a pre-populated test roster.
for (const threadRootId of [undefined, "f".repeat(64)])
  it(`adds a selected local agent only on Send, then sends after membership confirmation (thread=${!!threadRootId})`, async () => {
    const f = controlFixture();
    f.agent.pubkey = first.pubkey;
    const control = createAgentControl(f.host);
    await control.refresh();
    const h = mount(
      {
        scope: `https://relay.example.test:${"d".repeat(64)}`,
        ...(threadRootId ? { threadRootId } : {}),
      },
      control,
    );
    const channel = {
      id: "channel",
      name: "General",
      channelType: "stream" as const,
      members: ["d".repeat(64)],
    };
    const listeners = new Set<() => void>();
    // An acknowledged old invitation does not replace this explicit addition.
    const invitation: OutgoingEvent = {
      acknowledged: true,
      delivery: "failed",
      event: {
        id: "c".repeat(64),
        pubkey: "d".repeat(64),
        kind: 9000,
        content: "",
        created_at: Math.floor(Date.now() / 1000) - 16 * 60,
        tags: [
          ["h", "channel"],
          ["p", first.pubkey],
        ],
      },
    };
    let operations: readonly OutgoingEvent[] = [invitation];
    const retry = vi.fn();
    const add = vi.fn((input) => {
      operations = [
        {
          event: {
            ...input,
            pubkey: "d".repeat(64),
            id: "e".repeat(64),
            created_at: Math.floor(Date.now() / 1000),
          },
          delivery: "sending",
        },
      ];
      return "e".repeat(64);
    });
    const list = { status: "ready", channels: [channel] };
    Object.assign(h.session, {
      channels: { list: () => list, subscribeList: () => () => {} },
      viewer: "d".repeat(64),
      archives: { state: () => "not-archived" },
      workSessions: {
        refreshMembership: vi.fn(async () => {
          if (operations[0]?.delivery === "accepted")
            channel.members = [...channel.members, first.pubkey];
          return channel;
        }),
      },
      outbox: {
        supports: () => true,
        send: add,
        retry,
        ready: async () => {},
        recover: async () => {},
        acknowledge: async () => {},
        snapshot: () => operations,
        subscribe: (fn: () => void) => {
          listeners.add(fn);
          return () => listeners.delete(fn);
        },
      },
    });
    Object.assign(h.session, {
      memberAdditions: createMemberAdditions(
        new AbortController().signal,
        (channelId, key, intent) =>
          addChannelMember(
            h.session,
            channelId,
            key,
            new AbortController().signal,
            intent,
          ),
        vi.fn(),
      ),
    });
    try {
      fireEvent.click(screen.getByRole("button", { name: "First Honey" }));
      expect(add).not.toHaveBeenCalled();
      fireEvent.submit(screen.getByRole("form"));
      await act(async () => {});
      expect(add).not.toHaveBeenCalled();
      fireEvent.click(screen.getByRole("button", { name: "Invite" }));
      await act(async () => {});
      expect(add).toHaveBeenCalledWith(
        {
          kind: 9000,
          content: "",
          tags: [
            ["h", "channel"],
            ["p", first.pubkey],
            ["role", "bot"],
          ],
        },
        { key: `member-add:channel:${first.pubkey}`, value: "1" },
      );
      expect(h.messages.send).not.toHaveBeenCalled();
      expect(h.messages.reply).not.toHaveBeenCalled();
      await act(async () => {
        operations = operations.map((item) => ({
          ...item,
          delivery: "accepted",
        }));
        for (const listener of listeners) listener();
      });
      const send = threadRootId ? h.messages.reply : h.messages.send;
      expect(send).toHaveBeenCalledOnce();
      expect(retry).not.toHaveBeenCalled();
      expect(send.mock.calls[0]?.[threadRootId ? 3 : 2]).toEqual([
        first.pubkey,
      ]);
      // Native evidence now shares the ordinary remember-agent classification.
      expect(h.input().value).toBe("@Honey ");
    } finally {
      control.dispose();
    }
  });

it.each([false, true])(
  "keeps the selected draft on an enrollment error (expired=%s) without sending the message",
  async (expired) => {
    const f = controlFixture();
    f.agent.pubkey = first.pubkey;
    const control = createAgentControl(f.host);
    await control.refresh();
    const h = mount(
      { scope: `https://relay.example.test:${"d".repeat(64)}` },
      control,
    );
    const add = vi.fn(() => {
      throw new Error("Cannot add agent");
    });
    const list = {
      status: "ready",
      channels: [
        { id: "channel", channelType: "stream", members: ["d".repeat(64)] },
      ],
    };
    const operations = expired
      ? [
          {
            delivery: "failed",
            event: {
              id: "e".repeat(64),
              kind: 9000,
              created_at: Math.floor(Date.now() / 1000) - 16 * 60,
              tags: [
                ["h", "channel"],
                ["p", first.pubkey],
                ["role", "bot"],
              ],
            },
          },
        ]
      : [];
    Object.assign(h.session, {
      channels: { list: () => list, subscribeList: () => () => {} },
      viewer: "d".repeat(64),
      archives: { state: () => "not-archived" },
      workSessions: { refreshMembership: async () => list.channels[0] },
      outbox: {
        supports: () => true,
        send: add,
        snapshot: () => operations,
        ready: async () => {},
        recover: async () => {},
        acknowledge: async () => {},
      },
    });
    Object.assign(h.session, {
      memberAdditions: createMemberAdditions(
        new AbortController().signal,
        (channelId, key, intent) =>
          addChannelMember(
            h.session,
            channelId,
            key,
            new AbortController().signal,
            intent,
          ),
        vi.fn(),
      ),
    });
    try {
      fireEvent.click(screen.getByRole("button", { name: "First Honey" }));
      fireEvent.submit(screen.getByRole("form"));
      await act(async () => {});
      fireEvent.click(screen.getByRole("button", { name: "Invite" }));
      await act(async () => {});
      expect(screen.getByRole("alert")).toHaveTextContent(
        expired ? "addition expired" : "Cannot add agent",
      );
      expect(
        (screen.getByRole("textbox", { hidden: true }) as HTMLInputElement)
          .value,
      ).toBe("@Honey ");
      expect(h.messages.send).not.toHaveBeenCalled();
      if (expired) expect(add).not.toHaveBeenCalled();
      fireEvent.click(screen.getByRole("button", { name: "Close" }));
      await act(async () => {});
      expect(h.input()).not.toHaveAttribute("aria-disabled", "true");
    } finally {
      control.dispose();
    }
  },
);

it.each(
  ["send", "unmount", "disabled", "denied"].flatMap((outcome) =>
    ["mention", "text-and-mention"].flatMap((recipient) =>
      [true, false].map((parent) => ({
        outcome,
        recipient,
        parent,
      })),
    ),
  ),
)(
  "waits for agent admission before saved session messages: $recipient / $outcome / parent=$parent",
  async ({ outcome, recipient, parent }) => {
    const view = mount();
    const list = {
      status: "ready",
      channels: [
        {
          id: "channel",
          channelType: "session",
          ...(parent ? { parentChannelId: "parent" } : {}),
          members: [] as string[],
        },
      ],
    };
    const library = { status: "ready", definitions: [], identities: [first] };
    let release = () => {};
    const addAgents = vi.fn(
      () =>
        new Promise<void>((resolve, reject) => {
          release = () => {
            if (outcome === "denied") reject(new Error("Cannot add agents"));
            else {
              list.channels[0]?.members.push(first.pubkey);
              resolve();
            }
          };
        }),
    );
    const session = {
      ...view.session,
      channels: { list: () => list, subscribeList: () => () => {} },
      agentLibrary: {
        snapshot: () => library,
        subscribe: () => () => {},
        refresh: async () => {},
        retain: () => () => {},
      },
      workSessions: {
        addAgents,
        refreshMembership: vi.fn(async () => list.channels[0]),
      },
    } as unknown as RelaySession;
    view.retarget({ session, sessionConversation: true });
    expect(view.commands().inviteAgents).toBe(true);
    if (recipient === "mention") {
      await view.user.click(
        screen.getByRole("button", { name: "First Honey" }),
      );
    } else {
      await view.user.type(view.input(), "Hello Honey ");
      await view.user.click(
        screen.getByRole("button", { name: "First Honey" }),
      );
    }
    expect(addAgents).not.toHaveBeenCalled();
    expect(session.workSessions.refreshMembership).not.toHaveBeenCalled();
    expect(view.messages.send).not.toHaveBeenCalled();
    view.submit();
    await waitFor(() =>
      expect(addAgents).toHaveBeenCalledWith(
        "channel",
        [first.pubkey],
        expect.any(Function),
      ),
    );
    expect(view.messages.send).not.toHaveBeenCalled();
    if (outcome === "unmount") view.unmount();
    if (outcome === "disabled") view.retarget({ disabled: true });
    await act(async () => release());
    if (outcome === "send") {
      await waitFor(() => expect(view.messages.send).toHaveBeenCalledOnce());
      expect(addAgents).toHaveBeenCalledOnce();
    } else expect(view.messages.send).not.toHaveBeenCalled();
    if (outcome === "denied") {
      expect(await screen.findByRole("alert")).toHaveTextContent(
        "Cannot add agents",
      );
      expect(view.input()).toHaveTextContent("Honey");
    }
  },
);

it.each(
  [undefined, "root"].flatMap((root) =>
    [false, true].map((removeMention) => ({ root, removeMention })),
  ),
)(
  "resolves a sole session agent before send/reply: root=$root, removed=$removeMention",
  async ({ root, removeMention }) => {
    const view = mount();
    const channel = {
      id: "channel",
      channelType: "session" as const,
      members: [first.pubkey],
    };
    const list = { status: "ready" as const, channels: [channel] };
    const library = {
      status: "ready" as const,
      definitions: [],
      identities: [first],
    };
    const session = {
      ...view.session,
      viewer: "viewer",
      channels: { list: () => list, subscribeList: () => () => {} },
      agentLibrary: {
        snapshot: () => library,
        subscribe: () => () => {},
        refresh: vi.fn(async () => {}),
      },
      workSessions: {
        refreshMembership: vi.fn(async () => channel),
        addAgents: vi.fn(async () => {}),
      },
    } as unknown as RelaySession;
    view.retarget({
      session,
      sessionConversation: true,
      ...(root ? { threadRootId: root } : {}),
    });
    view.fill("Keep going ");
    if (removeMention) {
      await view.user.click(
        screen.getByRole("button", { name: "First Honey" }),
      );
      const remove = screen.getByRole("button", {
        name: `Remove mention Honey ${first.pubkey}`,
      });
      await view.user.hover(remove);
      expect(await screen.findByRole("tooltip")).toHaveTextContent(
        "Remove explicit mention of Honey (aaaaaaaa)",
      );
      await view.user.click(remove);
      expect(view.input()).toHaveValue("Keep going @Honey ");
      expect(view.input().querySelector(".inline-chip")).toBeNull();
      expect(
        screen.queryByRole("region", { name: "Explicit mentions" }),
      ).not.toBeInTheDocument();
    }
    view.submit();
    await waitFor(() =>
      expect(
        root ? view.messages.reply : view.messages.send,
      ).toHaveBeenCalled(),
    );
    if (root)
      expect(view.messages.reply).toHaveBeenCalledExactlyOnceWith(
        "channel",
        root,
        removeMention ? "Keep going @Honey " : "Keep going ",
        [first.pubkey],
        [],
      );
    else
      expect(view.messages.send).toHaveBeenCalledExactlyOnceWith(
        "channel",
        removeMention ? "Keep going @Honey " : "Keep going ",
        [first.pubkey],
        [],
      );
    expect(session.workSessions.addAgents).not.toHaveBeenCalled();
  },
);

it("requires explicit recipients when an ordinary session has multiple agents", async () => {
  const view = mount();
  const list = {
    status: "ready",
    channels: [
      {
        id: "channel",
        channelType: "session",
        members: [first.pubkey, second.pubkey],
      },
    ],
  };
  const session = {
    ...view.session,
    channels: { list: () => list, subscribeList: () => () => {} },
    workSessions: {
      refreshMembership: vi.fn(async () => list.channels[0]),
      addAgents: vi.fn(async () => {}),
    },
  } as unknown as RelaySession;
  view.retarget({ session, sessionConversation: true });
  expect(screen.queryByRole("button", { name: "Choose an agent" })).toBeNull();
  await view.user.click(screen.getByRole("button", { name: "First Honey" }));
  view.submit();
  await waitFor(() =>
    expect(view.messages.send).toHaveBeenLastCalledWith(
      "channel",
      expect.any(String),
      [first.pubkey],
      [],
    ),
  );
  expect(session.workSessions.addAgents).not.toHaveBeenCalled();
});

it.each(["ready", "failed", "unmounted"])(
  "refreshes cached membership before sending an existing mention: %s",
  async (outcome) => {
    const view = mount();
    const channel = {
      id: "channel",
      channelType: "session",
      members: [first.pubkey],
    };
    const list = { status: "ready", channels: [channel] };
    const library = { status: "ready", identities: [first] };
    let release = () => {};
    const refreshMembership = vi.fn(
      () =>
        new Promise<typeof channel>((resolve, reject) => {
          release = () =>
            outcome === "failed"
              ? reject(new Error("Could not refresh channel membership"))
              : resolve(channel);
        }),
    );
    const addAgents = vi.fn();
    const session = {
      ...view.session,
      channels: { list: () => list, subscribeList: () => () => {} },
      agentLibrary: { snapshot: () => library, subscribe: () => () => {} },
      workSessions: { refreshMembership, addAgents },
    } as unknown as RelaySession;
    view.retarget({ session, sessionConversation: true });
    await view.user.click(screen.getByRole("button", { name: "First Honey" }));
    view.submit();
    await waitFor(() =>
      expect(refreshMembership).toHaveBeenCalledWith("channel"),
    );
    expect(view.messages.send).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Send message" })).toBeDisabled();
    if (outcome === "unmounted") view.unmount();
    await act(async () => release());
    expect(addAgents).not.toHaveBeenCalled();
    if (outcome === "ready") expect(view.messages.send).toHaveBeenCalledOnce();
    else expect(view.messages.send).not.toHaveBeenCalled();
    if (outcome === "failed") {
      expect(screen.getByRole("alert")).toHaveTextContent("Could not refresh");
      expect(view.input()).toHaveTextContent("Honey");
      expect(
        screen.getByRole("button", { name: "Send message" }),
      ).toBeEnabled();
    }
  },
);

it("keeps retry submission available while a new-session draft is locked", () => {
  const submit = vi.fn();
  const h = mount({
    submission: {
      draftKey: "session-retry",
      initialDraft: "Keep this operation",
      locked: true,
      disabled: false,
      submit,
    },
  });
  expect(document.querySelector("[data-reserve-typing]")).toBeNull();
  expect(h.input()).toHaveAttribute("aria-disabled", "true");
  const send = screen.getByRole("button", { name: "Send message" });
  expect(send).toBeEnabled();
  fireEvent.click(send);
  expect(submit).toHaveBeenCalledWith({
    text: "Keep this operation",
    recipients: [],
  });
  expect(h.messages.send).not.toHaveBeenCalled();
});

it.each([undefined, "root"])(
  "prefills only exact selected agents after an accepted send in %s",
  (threadRootId) => {
    const h = mount(threadRootId ? { threadRootId } : {});
    vi.spyOn(h.session.profiles, "snapshot").mockReturnValue(
      new Map([[second.pubkey, { name: "Honey", isAgent: true }]]),
    );
    act(() => {
      h.commands().insertMention(first);
      h.commands().insertMention(second);
      h.commands().insertMention(second);
      h.commands().insertText("hello");
    });
    const send = threadRootId ? h.messages.reply : h.messages.send;
    send.mockImplementationOnce(() => {
      throw new Error("outbox full");
    });
    h.submit();
    expect(h.input()).toHaveValue("@Honey @Honey @Honey hello");
    h.submit();
    expect(send.mock.calls.at(-1)?.[threadRootId ? 3 : 2]).toEqual([
      first.pubkey,
      second.pubkey,
      second.pubkey,
    ]);
    expect(h.input()).toHaveValue("@Honey ");
    const recipients = screen.getByRole("region", {
      name: "Explicit mentions",
    });
    expect(within(recipients).getAllByRole("button")).toHaveLength(1);
    expect(
      within(recipients).getByRole("button", {
        name: `Remove mention Honey ${second.pubkey}`,
      }),
    ).toBeVisible();
    expect(
      within(h.input()).getAllByRole("img", { name: "Agent Honey" }),
    ).toHaveLength(1);
    expect(
      h.input().querySelector("button, a, [tabindex], [title]"),
    ).toBeNull();
    h.retarget({ channelId: "other" });
    expect(h.input()).toHaveValue("");
    h.retarget({ channelId: "channel" });
    expect(h.input()).toHaveValue("@Honey ");
    h.submit();
    expect(send.mock.calls.at(-1)?.[threadRootId ? 3 : 2]).toEqual([
      second.pubkey,
    ]);
    h.input().setSelectionRange(0, 6);
    act(() => {
      h.commands().insertText("Honey");
    });
    expect(h.input()).toHaveValue("Honey ");
    expect(within(h.input()).queryByRole("img")).not.toBeInTheDocument();
    h.submit();
    expect(send.mock.calls.at(-1)?.[threadRootId ? 3 : 2]).toEqual([]);
    expect(h.input()).toHaveValue("");
  },
);

it("opt-out changes future prefills, not the current draft, and re-enable revives nothing", () => {
  const h = mount();
  vi.spyOn(h.session.profiles, "snapshot").mockReturnValue(
    new Map([[second.pubkey, { name: "Honey", isAgent: true }]]),
  );
  act(() => {
    h.commands().insertMention(second);
  });
  h.submit();
  expect(h.input()).toHaveValue("@Honey ");
  setRememberAgentsPreference(false);
  expect(h.input()).toHaveValue("@Honey ");
  h.submit();
  expect(h.messages.send.mock.calls.at(-1)?.[2]).toEqual([second.pubkey]);
  expect(h.input()).toHaveValue("");
  setRememberAgentsPreference(true);
  expect(h.input()).toHaveValue("");
});

it.each([
  {},
  { threadRootId: "thread" },
  { threadRootId: "thread", mediaTimeSeconds: 12 },
])(
  "disables nonmember channel/thread/media composers and follows membership changes: %j",
  (destination) => {
    const h = mount(destination);
    const listeners = new Set<() => void>();
    let list: ReturnType<RelaySession["channels"]["list"]> = {
      status: "ready",
      channels: [],
    };
    h.retarget({
      session: {
        ...h.session,
        channels: {
          window: () => {
            throw new Error("Unused fixture window");
          },
          subscribeWindow: () => () => {},
          ensureList() {},
          ensure() {},
          loadOlder() {},
          list: () => list,
          get: () => ({ id: "channel", name: "Public", readOnly: true }),
          subscribeList: (listener) => {
            listeners.add(listener);
            return () => {
              listeners.delete(listener);
            };
          },
        } as RelaySession["channels"],
      },
    });
    expect(h.input()).toHaveAttribute("aria-disabled", "true");
    fireEvent.keyDown(h.input(), { key: "Enter" });
    expect(h.messages.send).not.toHaveBeenCalled();
    expect(h.messages.reply).not.toHaveBeenCalled();
    act(() => {
      list = { status: "ready", channels: [{ id: "channel", name: "Joined" }] };
      for (const listener of listeners) listener();
    });
    expect(h.input()).not.toHaveAttribute("aria-disabled", "true");
    act(() => {
      list = { status: "ready", channels: [] };
      for (const listener of listeners) listener();
    });
    expect(h.input()).toHaveAttribute("aria-disabled", "true");
  },
);

it("keeps inline recipient identity and source stable through directory collision changes", async () => {
  const h = mount();
  const listeners = new Set<() => void>();
  let identities = [first, second];
  const provider = createAgentDirectory();
  const names = bindNames(
    {
      profiles: h.session.profiles,
      agentLibrary: {
        snapshot: () => ({ status: "ready", definitions: [], identities }),
        subscribe: (listener) => {
          listeners.add(listener);
          return () => {
            listeners.delete(listener);
          };
        },
        refresh: async () => {},
        retain: () => () => {},
      },
    },
    { snapshot: () => [provider], subscribe: () => () => {} },
  );
  h.retarget({ session: { ...h.session, names } });
  await h.user.click(screen.getByRole("button", { name: "First Honey" }));
  await h.user.click(screen.getByRole("button", { name: "Second Honey" }));
  const chips = () => within(h.input()).getAllByRole("img");
  const labels = () => chips().map((chip) => chip.textContent);
  expect(labels()).toEqual(["@Honey · rcaj", "@Honey · 04hu"]);
  const source = h.input().value;
  act(() => {
    identities = [first, { ...second, name: "Renamed Honey" }];
    for (const notify of listeners) notify();
  });
  // Labels follow live facts; authored source and recipients do not change.
  expect(names.resolve(second.pubkey)).toBe("Renamed Honey");
  expect(labels()).toEqual(["@Honey", "@Renamed Honey"]);
  expect(h.input()).toHaveValue(source);
  act(() => {
    identities = [first, second];
    for (const notify of listeners) notify();
  });
  expect(names.lookup(first.pubkey)?.qualifier).toBeTruthy();
  expect(labels()).toEqual(["@Honey · rcaj", "@Honey · 04hu"]);
  h.input().setSelectionRange(7, 13);
  act(() => h.commands().insertText(""));
  // Removing a selected chip does not remove the other channel member from naming scope.
  expect(labels()).toEqual(["@Honey · rcaj"]);
  h.submit();
  expect(h.messages.send.mock.calls[0]?.[2]).toEqual([first.pubkey]);
  h.unmount();
  names.dispose();
});

it.each([false, true])(
  "asks before mentioning a removed person in an untyped channel without enrolling anyone (mixed native=%s)",
  async (mixed) => {
    const viewer = keypair(),
      relay = keypair();
    const scope = `https://relay.example.test:${viewer.pubkey}`;
    const f = controlFixture();
    f.agent.pubkey = second.pubkey;
    const native = createAgentControl(f.host);
    await native.refresh();
    let members = [viewer.pubkey, first.pubkey];
    let time = 1700000000;
    const sign = vi.fn(async (template: EventTemplate) =>
      signed(viewer, template),
    );
    const publish = vi.fn(async () => {});
    const readLibrary = vi.fn(async () => ({
      definitions: [],
      identities: [],
    }));
    const owner = createRelaySession(
      {
        viewer: viewer.pubkey,
        relayAuthor: relay.pubkey,
        scope: "https://relay.example.test",
        media: () => undefined,
        query: async (filters) =>
          filters.flatMap((filter) =>
            filter.kinds?.includes(39002)
              ? [roster(relay, "channel", members, time)]
              : filter.kinds?.includes(39000)
                ? [metadata(relay, "channel", "General")]
                : [],
          ),
        readAgentLibrary: readLibrary,
        writer: { kinds: [9, 9000], sign, publish },
      },
      { outboxStorage: { load: () => [], save() {} }, agentChoices: native },
    );
    const refresh = () =>
      owner.session.read(
        [
          { kinds: [39002], "#d": ["channel"], limit: 1 },
          { kinds: [39000], "#d": ["channel"], limit: 1 },
        ],
        { fresh: true },
      );
    await refresh();
    const h = mount({ session: owner.session, scope }, native);
    try {
      fireEvent.click(screen.getByRole("button", { name: "First Honey" }));
      if (mixed)
        fireEvent.click(screen.getByRole("button", { name: "Second Honey" }));
      const draft = h.input().value;
      members = [viewer.pubkey];
      time++;
      await act(refresh);
      // An untyped channel is an ordinary channel: a removed recipient is now
      // outside it, so the sender chooses. Nothing enrolls or sends meanwhile.
      h.submit();
      expect(screen.getByRole("dialog")).toHaveTextContent(
        mixed
          ? "Honey, Honey are not in this channel."
          : "Honey is not in this channel.",
      );
      await userEvent.setup().keyboard("{Escape}");
      await waitFor(() =>
        expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
      );
      expect(h.input()).toHaveValue(draft);
      expect(sign).not.toHaveBeenCalled();
      expect(publish).not.toHaveBeenCalled();
      expect(readLibrary).not.toHaveBeenCalled();
    } finally {
      h.unmount();
      owner.dispose();
      native.dispose();
    }
  },
);

it("retargets within a thread without losing its draft and sends the selected parent", () => {
  const h = mount({ threadRootId: "root" });
  h.fill("keep this draft");
  const input = h.input();
  h.retarget({ threadRootId: "root", replyParentId: "parent" });
  expect(h.input()).toBe(input);
  expect(h.input()).toHaveValue("keep this draft");
  fireEvent.keyDown(h.input(), { key: "Enter" });
  expect(h.messages.reply).toHaveBeenCalledExactlyOnceWith(
    "channel",
    "root",
    "keep this draft",
    [],
    [],
    "parent",
  );
});

it("toggles the whole draft spoiler with a collapsed caret and preserves selection/history", () => {
  const h = mount();
  h.fill("secret");
  act(() => h.input().toggleFormat("spoiler"));
  expect(h.input().querySelector("[data-spoiler]")).toHaveTextContent("secret");
  expect(h.input().selectionStart).toBe(6);
  expect(h.input().selectionEnd).toBe(6);
  act(() => h.input().undo(false));
  expect(h.input().querySelector("[data-spoiler]")).toBeNull();
  act(() => h.input().undo(true));
  h.submit();
  expect(h.messages.send).toHaveBeenCalledWith("channel", "||secret||", [], []);
});

const editableMessage = (
  overrides: Partial<ChannelMessage> = {},
): ChannelMessage => ({
  id: "c".repeat(64),
  channelId: "channel",
  authorId: first.pubkey,
  createdAt: 10,
  content: "Original message",
  replyCount: 0,
  participants: [],
  mentions: [],
  attachments: [],
  reactions: [],
  ...overrides,
});

it("edits in the same composer, cancels without persisting edit text, and restores draft undo history", () => {
  const h = mount({}, undefined, first.pubkey);
  h.setRows([editableMessage()]);
  h.fill("Unsent draft");
  h.fill("");
  const input = h.input();
  fireEvent.keyDown(input, { key: "ArrowUp" });
  expect(h.input()).toBe(input);
  expect(input).toHaveAccessibleName("Edit message");
  expect(input).toHaveValue("Original message");
  h.fill("Temporary edit");
  expect(readView("scope", "draft:channel", null)).toMatchObject({ text: "" });
  fireEvent.keyDown(input, { key: "Escape", keyCode: 27 });
  expect(input).toHaveValue("");
  expect(input).toHaveFocus();
  fireEvent.keyDown(input, { key: "z", ctrlKey: true });
  expect(input).toHaveValue("Unsent draft");
  expect(h.messages.edit).not.toHaveBeenCalled();
});

it.each(["cancel", "accepted"])(
  "restores rich draft history and mention provenance after an edit is %s",
  (finish) => {
    const h = mount({}, undefined, first.pubkey);
    h.setRows([editableMessage()]);
    act(() => h.commands().insertMention(second));
    act(() => {
      h.input().setSelectionRange(0, 6);
      h.input().toggleFormat("bold");
    });
    const formatted = h.input().innerHTML;
    h.fill("");
    fireEvent.keyDown(h.input(), { key: "ArrowUp" });
    // The temporary editor must not inherit the unsent draft's undo stack.
    act(() => h.input().undo(false));
    expect(h.input()).toHaveValue("Original message");
    h.fill("Temporary edit");
    if (finish === "cancel") fireEvent.keyDown(h.input(), { key: "Escape" });
    else {
      h.submit();
      h.setDelivery("accepted");
    }
    act(() => h.input().undo(false));
    expect(h.input().innerHTML).toBe(formatted);
    expect(
      screen.getByRole("region", { name: "Explicit mentions" }),
    ).toBeVisible();
    h.submit();
    expect(h.messages.send).toHaveBeenCalledWith(
      "channel",
      "**@Honey** ",
      [second.pubkey],
      [],
    );
  },
);

it.each([
  ["bold", "**Revised**"],
  ["spoiler", "||Revised||"],
  ["code", "`Revised`"],
  ["bullet_list", "- Revised"],
] as const)(
  "serializes %s formatting when saving an edit",
  (format, markdown) => {
    const h = mount({}, undefined, first.pubkey);
    h.setRows([editableMessage()]);
    fireEvent.keyDown(h.input(), { key: "ArrowUp" });
    h.fill("Revised");
    act(() => {
      h.input().setSelectionRange(0, 7);
      h.input().toggleFormat(format);
    });
    h.submit();
    expect(h.messages.edit).toHaveBeenCalledExactlyOnceWith(
      "c".repeat(64),
      markdown,
      "c".repeat(64),
    );
    expect(h.messages.send).not.toHaveBeenCalled();
    expect(readView("scope", "draft:channel", "")).toBe("");
  },
);

it("continues a list while editing and saves through Save changes", async () => {
  const h = mount({}, undefined, first.pubkey);
  h.setRows([editableMessage()]);
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  h.fill("Revised");
  act(() => {
    h.input().setSelectionRange(7, 7);
    h.input().toggleFormat("bullet_list");
  });
  await h.user.keyboard("{Enter}Added");
  expect(h.input().querySelectorAll("ul > li")).toHaveLength(2);
  expect(h.messages.edit).not.toHaveBeenCalled();
  await h.user.click(screen.getByRole("button", { name: "Save changes" }));
  expect(h.messages.edit).toHaveBeenCalledExactlyOnceWith(
    "c".repeat(64),
    "- Revised\n- Added",
    "c".repeat(64),
  );
});

it.each(["bullet_list", "ordered_list", "code_block"] as const)(
  "does not save a whitespace-only %s edit through Enter",
  (format) => {
    const h = mount({}, undefined, first.pubkey);
    h.setRows([editableMessage()]);
    fireEvent.keyDown(h.input(), { key: "ArrowUp" });
    h.fill(" ");
    act(() => h.input().toggleFormat(format));
    expect(screen.getByRole("button", { name: "Save changes" })).toBeDisabled();
    fireEvent.keyDown(h.input(), { key: "Enter", keyCode: 13 });
    expect(h.messages.edit).not.toHaveBeenCalled();
    expect(screen.getByText("Editing message")).toBeVisible();
  },
);

it("saves an edited fenced message on Enter instead of opening a block from its closing fence", () => {
  const h = mount({}, undefined, first.pubkey);
  h.setRows([editableMessage({ content: "```js\ncode\n```" })]);
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  expect(h.input()).toHaveValue("```js\ncode\n```");
  act(() => {
    h.input().setSelectionRange(10, 10);
    h.input().insertText("!");
    const end = h.input().value.length;
    h.input().setSelectionRange(end, end);
  });
  fireEvent.keyDown(h.input(), { key: "Enter", keyCode: 13 });
  expect(h.input().querySelector("pre")).toBeNull();
  expect(h.messages.edit).toHaveBeenCalledExactlyOnceWith(
    "c".repeat(64),
    "```js\ncode!\n```",
    "c".repeat(64),
  );
  expect(h.messages.send).not.toHaveBeenCalled();
});

it("saves only once, locks until delivery, and restores the new-message composer on acceptance", () => {
  const h = mount({}, undefined, first.pubkey);
  const row = editableMessage();
  h.setRows([row]);
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  h.fill("Revised message");
  act(() => {
    h.submit();
    h.submit();
  });
  expect(h.messages.edit).toHaveBeenCalledExactlyOnceWith(
    row.id,
    "Revised message",
    row.id,
  );
  expect(h.messages.send).not.toHaveBeenCalled();
  expect(h.input()).toHaveAttribute("contenteditable", "false");
  expect(screen.getByRole("button", { name: "Close edit" })).toBeDisabled();
  h.setDelivery("accepted");
  expect(screen.queryByText("Editing message")).not.toBeInTheDocument();
  expect(h.input()).toHaveValue("");
  expect(h.input()).toHaveFocus();
  expect(h.onSend).not.toHaveBeenCalled();
});

it.each(["failed", "unknown"] as const)(
  "keeps a %s edit and retries the same operation, not a new message",
  (delivery) => {
    const h = mount({}, undefined, first.pubkey);
    h.setRows([editableMessage()]);
    fireEvent.keyDown(h.input(), { key: "ArrowUp" });
    h.fill("Revised");
    h.submit();
    h.setDelivery(delivery);
    expect(h.input()).toHaveValue("Revised");
    expect(h.input()).toHaveAttribute("contenteditable", "false");
    fireEvent.click(screen.getByRole("button", { name: "Retry edit" }));
    expect(h.session.outbox?.retry).toHaveBeenCalledExactlyOnceWith("edit-id");
    expect(h.messages.edit).toHaveBeenCalledTimes(1);
    h.setDelivery("seen");
    expect(h.input()).toHaveValue("");
  },
);

it.each(["changed", "deleted"])(
  "preserves text and refuses to overwrite a %s target",
  (state) => {
    const h = mount({}, undefined, first.pubkey);
    const row = editableMessage();
    h.setRows([row]);
    fireEvent.keyDown(h.input(), { key: "ArrowUp" });
    h.fill("My changes");
    h.setRows(
      state === "deleted"
        ? []
        : [{ ...row, content: "Another client edited this" }],
    );
    h.submit();
    expect(screen.getByRole("alert")).toHaveTextContent(
      state === "deleted"
        ? "no longer available"
        : "changed while you were editing",
    );
    expect(h.input()).toHaveValue("My changes");
    expect(h.messages.edit).not.toHaveBeenCalled();
  },
);

it.each([false, true])(
  "does not publish blank or unchanged content and preserves attachment source (mention: %s)",
  (mention) => {
    const h = mount({}, undefined, first.pubkey);
    h.setProfiles(
      new Map([[second.pubkey, { id: second.pubkey, name: "Honey" }]]),
    );
    const caption = mention ? "@Honey Caption" : "Caption";
    const sourceContent = `${caption}\n\n[report.pdf](https://files.test/report.pdf)`;
    h.setRows([
      editableMessage({
        content: caption,
        sourceContent,
        mentions: mention ? [second.pubkey] : [],
      }),
    ]);
    fireEvent.keyDown(h.input(), { key: "ArrowUp" });
    const seed = h.input().value;
    expect(seed).toBe(
      mention
        ? sourceContent.replace(
            "@Honey",
            `[@Honey](${profileTarget(second.pubkey)})`,
          )
        : sourceContent,
    );
    h.fill(" ");
    h.submit();
    expect(screen.getByText("Editing message")).toBeVisible();
    h.fill(seed);
    fireEvent.keyDown(h.input(), { key: "Enter", keyCode: 13 });
    expect(screen.queryByText("Editing message")).not.toBeInTheDocument();
    expect(h.input()).toHaveValue("");
    expect(h.messages.edit).not.toHaveBeenCalled();
    expect(h.messages.send).not.toHaveBeenCalled();
  },
);

it.each(["changed", "deleted"])(
  "keeps an unchanged mention edit open when the target is %s",
  (state) => {
    const h = mount({}, undefined, first.pubkey);
    h.setProfiles(
      new Map([[second.pubkey, { id: second.pubkey, name: "Honey" }]]),
    );
    const row = editableMessage({
      content: "@Honey hello",
      mentions: [second.pubkey],
    });
    h.setRows([row]);
    fireEvent.keyDown(h.input(), { key: "ArrowUp" });
    const seed = h.input().value;
    h.setRows(
      state === "deleted"
        ? []
        : [{ ...row, content: "Another client edited this" }],
    );
    h.submit();
    expect(screen.getByRole("alert")).toHaveTextContent(
      state === "deleted"
        ? "no longer available"
        : "changed while you were editing",
    );
    expect(screen.getByText("Editing message")).toBeVisible();
    expect(h.input()).toHaveValue(seed);
    expect(h.messages.edit).not.toHaveBeenCalled();
  },
);

it.each([
  { shiftKey: true },
  { altKey: true },
  { ctrlKey: true },
  { metaKey: true },
  { repeat: true },
  { isComposing: true },
  { keyCode: 229 },
])("does not take over modified/repeated/composing ArrowUp: %j", (keys) => {
  const h = mount({}, undefined, first.pubkey);
  h.setRows([editableMessage()]);
  fireEvent.keyDown(h.input(), { key: "ArrowUp", ...keys });
  expect(h.input()).toHaveValue("");
  expect(screen.queryByText("Editing message")).not.toBeInTheDocument();
});

it("does not replace a nonempty draft and confines channel/thread targets to their owner", () => {
  const h = mount({}, undefined, first.pubkey);
  const channelRow = editableMessage();
  const reply = editableMessage({
    id: "d".repeat(64),
    content: "Thread reply",
    threadRootId: channelRow.id,
  });
  h.setRows([channelRow, reply]);
  h.fill("Draft");
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  expect(h.input()).toHaveValue("Draft");
  h.fill("");
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  expect(h.input()).toHaveValue(channelRow.content);
  h.retarget({
    threadRootId: channelRow.id,
    editMessages: [channelRow, reply],
  });
  expect(h.input()).toHaveValue("");
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  expect(h.input()).toHaveValue(reply.content);
  h.fill("Unsubmitted edit");
  h.retarget({ channelId: "other", threadRootId: "another", editMessages: [] });
  expect(h.input()).toHaveValue("");
  expect(h.messages.edit).not.toHaveBeenCalled();
});

it.each([false, true])(
  "renders preserved mention links as chips when editing (already edited: %s)",
  (edited) => {
    const h = mount({}, undefined, first.pubkey);
    h.setProfiles(
      new Map([[second.pubkey, { id: second.pubkey, name: "Honey" }]]),
    );
    const link = `[@Honey](${profileTarget(second.pubkey)})`;
    const content = `${edited ? link : "@Honey"} whats your name`;
    h.setRows([
      editableMessage({
        content,
        mentions: [second.pubkey],
        ...(edited ? { edited: true as const } : {}),
      }),
    ]);
    fireEvent.keyDown(h.input(), { key: "ArrowUp" });
    expect(h.input()).toHaveTextContent("Honey whats your name");
    expect(h.input().textContent).not.toContain("nostr:");
    expect(h.input().value).toBe(`${link} whats your name`);
    expect(
      screen.queryByRole("region", { name: "Explicit mentions" }),
    ).not.toBeInTheDocument();
    act(() => h.commands().insertText("?"));
    h.submit();
    expect(h.messages.edit).toHaveBeenCalledWith(
      "c".repeat(64),
      `${link} whats your name?`,
      "c".repeat(64),
    );
    expect(h.messages.send).not.toHaveBeenCalled();
  },
);

it.each([
  (link: string) => `\`${link}\``,
  (link: string) => `\`\`\`\n${link}\n\`\`\``,
  () => "[@Honey](nostr:npub1invalid)",
  () => "@Honey without signed identity",
])("keeps literal or unbound edit text unchanged", (source) => {
  const h = mount({}, undefined, first.pubkey);
  const content = source(`[@Honey](${profileTarget(second.pubkey)})`);
  h.setRows([editableMessage({ content })]);
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  expect(h.input().textContent).toBe(content);
  expect(h.input().querySelector(".inline-chip")).toBeNull();
});

it("inserts readable literal mention links without new notification recipients during edits", () => {
  const h = mount({}, undefined, first.pubkey);
  h.setRows([editableMessage()]);
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  const target = profileTarget(second.pubkey);
  const names = [
    "Bee [x]",
    "A *B*",
    "A `B`",
    "A &amp; B",
    "Sam\n[Details](https://example.test/)",
  ];
  act(() => {
    h.commands().insertMention(second);
  });
  act(() => {
    h.commands().insertMentions(
      names.map((name, index) => ({ pubkey: `${index}`.repeat(64), name })),
    );
  });
  const labels = ["@Honey", ...names.map((name) => `@${name}`)].map((label) =>
    label.replace("\n", " "),
  );
  // Each selection stays one literal label on its own exact identity link.
  const readable = (body: string) => {
    expect(h.input().textContent).toBe(`Original message${labels.join(" ")} `);
    expect(h.input().querySelectorAll(".inline-chip")).toHaveLength(6);
    type Node = {
      type: string;
      url?: string;
      value?: string;
      children?: Node[];
    };
    const text = (node: Node): string =>
      node.value ?? node.children?.map(text).join("") ?? "";
    const links: { url: string | undefined; label: string }[] = [];
    const visit = (node: Node) => {
      if (node.type === "link")
        links.push({ url: node.url, label: text(node) });
      node.children?.forEach(visit);
    };
    visit(fromMarkdown(body));
    expect(links).toEqual([
      { url: target, label: "@Honey" },
      ...labels.slice(1).map((label, index) => ({
        url: profileTarget(`${index}`.repeat(64)),
        label,
      })),
    ]);
  };
  readable(h.input().value);
  expect(
    screen.queryByRole("region", { name: "Explicit mentions" }),
  ).not.toBeInTheDocument();
  h.submit();
  expect(h.messages.edit).toHaveBeenCalledOnce();
  const saved = h.messages.edit.mock.calls[0]?.[1] as string;
  expect(saved).toBe(h.input().value);
  expect(saved).not.toContain("\n");
  readable(saved);
  expect(h.messages.send).not.toHaveBeenCalled();
  // Reopening the saved edit keeps the same readable identity links.
  h.setRows([editableMessage({ content: saved, edited: true })]);
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  expect(h.input().value).toBe(saved);
  readable(saved);
});

const pasteText = (input: ComposerInputElement, text: string) =>
  act(() => {
    input.focus();
    fireEvent.paste(input, {
      clipboardData: {
        items: [],
        getData: (type: string) => (type === "text/plain" ? text : ""),
      },
    });
  });

it("notifies a pasted identity link for a member under their current name, not an unknown key", () => {
  const h = mount();
  h.setProfiles(new Map([[first.pubkey, { name: "Honey Bee" }]]));
  const eve = `[@Eve](${profileTarget("e".repeat(64))})`;
  pasteText(
    h.input(),
    `Ask [@Honey](${profileTarget(first.pubkey)}) and ${eve} `,
  );
  expect(h.input()).toHaveValue(`Ask @Honey Bee and ${eve} `);
  expect(
    within(
      screen.getByRole("region", { name: "Explicit mentions" }),
    ).getAllByRole("button"),
  ).toHaveLength(1);
  h.submit();
  expect(h.messages.send).toHaveBeenCalledExactlyOnceWith(
    "channel",
    `Ask @Honey Bee and ${eve} `,
    [first.pubkey],
    [],
  );
});

for (const channelType of ["stream", "forum"] as const)
  it(`offers a pasted nonmember with a known profile like the picker in ${channelType}`, async () => {
    const h = mount();
    const list = {
      status: "ready",
      channels: [{ id: "channel", channelType, members: ["d".repeat(64)] }],
    };
    Object.assign(h.session, {
      viewer: "d".repeat(64),
      channels: { list: () => list, subscribeList: () => () => {} },
      memberAdditions: { add: vi.fn() },
    });
    h.setProfiles(new Map([[first.pubkey, { name: "Honey Bee" }]]));
    // The pasted label never names the recipient; an uncached key stays display-only.
    const eve = `[@Eve](${profileTarget("e".repeat(64))})`;
    pasteText(
      h.input(),
      `Ask [@Jane](${profileTarget(first.pubkey)}) and ${eve} `,
    );
    expect(h.input()).toHaveValue(`Ask @Honey Bee and ${eve} `);
    expect(
      within(
        screen.getByRole("region", { name: "Explicit mentions" }),
      ).getAllByRole("button"),
    ).toHaveLength(1);
    fireEvent.submit(screen.getByRole("form"));
    expect(screen.getByRole("dialog")).toHaveTextContent(
      "Honey Bee is not in this channel. Invite them to the channel, or send without inviting them.",
    );
    fireEvent.click(screen.getByRole("button", { name: "Do nothing" }));
    await act(async () => {});
    expect(h.messages.send).toHaveBeenCalledOnce();
    expect(h.messages.send.mock.calls[0]?.slice(0, 3)).toEqual([
      "channel",
      `Ask @Honey Bee and ${eve} `,
      [],
    ]);
    expect(h.messages.send.mock.calls[0]?.at(-1)).toEqual([first.pubkey]);
  });

it("keeps pasted identity links display-only during edits", () => {
  const h = mount({}, undefined, first.pubkey);
  h.setRows([editableMessage()]);
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  const link = `[@Honey](${profileTarget(second.pubkey)})`;
  pasteText(h.input(), ` ${link} `);
  expect(h.input()).toHaveValue(`Original message ${link} `);
  expect(
    screen.queryByRole("region", { name: "Explicit mentions" }),
  ).not.toBeInTheDocument();
  h.submit();
  expect(h.messages.edit).toHaveBeenCalledExactlyOnceWith(
    "c".repeat(64),
    `Original message ${link} `,
    "c".repeat(64),
  );
});

it.each([
  { authorId: second.pubkey },
  { agentEnvelope: true as const },
  { diff: { filePath: "a.ts", truncated: false } },
  {
    membership: {
      type: "member_joined" as const,
      actor: first.pubkey,
      target: second.pubkey,
    },
  },
  { delivery: "sending" as const },
  { delivery: "failed" as const },
  { delivery: "unknown" as const },
])("skips a newer ineligible row: %j", (overrides) => {
  const h = mount({}, undefined, first.pubkey);
  h.setRows([
    editableMessage(),
    editableMessage({
      ...overrides,
      id: "d".repeat(64),
      content: "Ineligible",
    }),
  ]);
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  expect(h.input()).toHaveValue("Original message");
});

it("does not start editing without a viewer or edit capability", () => {
  const h = mount();
  h.setRows([editableMessage()]);
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  expect(h.input()).toHaveValue("");
  const outbox = h.session.outbox;
  if (!outbox) throw new Error("Missing fixture outbox");
  h.retarget({
    session: {
      ...h.session,
      viewer: first.pubkey,
      outbox: { ...outbox, supports: (kind) => kind === 9 },
    },
  });
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  expect(h.input()).toHaveValue("");
});

it("does not reopen a target with an unresolved edit after closing it", () => {
  const h = mount({}, undefined, first.pubkey);
  h.setRows([editableMessage()]);
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  h.fill("Waiting for delivery");
  h.submit();
  h.setDelivery("unknown");
  fireEvent.click(screen.getByRole("button", { name: "Close edit" }));
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  expect(h.input()).toHaveValue("");
  expect(screen.queryByText("Editing message")).not.toBeInTheDocument();
});

it("keeps the draft but blocks new messages while archived, then re-enables it on restore", async () => {
  const h = mount();
  let channel: ChannelSummary = { id: "channel", name: "General" };
  let list = { status: "ready" as const, channels: [channel] };
  const listeners = new Set<() => void>();
  h.retarget({
    session: {
      ...h.session,
      channels: {
        ...h.session.channels,
        get: () => channel,
        list: () => list,
        subscribeList: (listener) => {
          listeners.add(listener);
          return () => {
            listeners.delete(listener);
          };
        },
      },
    },
  });
  h.fill("Draft survives archive");
  const archive = (archived: true | undefined) =>
    act(() => {
      channel = {
        id: "channel",
        name: "General",
        ...(archived ? { archived } : {}),
      };
      list = { ...list, channels: [channel] };
      for (const listener of listeners) listener();
    });
  archive(true);
  expect(h.input()).toHaveAttribute("aria-disabled", "true");
  expect(h.input()).toHaveValue("Draft survives archive");
  expect(screen.getByRole("button", { name: "Send message" })).toBeDisabled();
  h.submit();
  expect(h.messages.send).not.toHaveBeenCalled();
  archive(undefined);
  expect(h.input()).not.toHaveAttribute("aria-disabled", "true");
  expect(h.input()).toHaveValue("Draft survives archive");
  h.submit();
  await waitFor(() => expect(h.messages.send).toHaveBeenCalledOnce());
});

it.each(["archived", "readOnly"] as const)(
  "blocks edit entry and save when channel becomes %s",
  (flag) => {
    const h = mount({}, undefined, first.pubkey);
    h.setRows([editableMessage()]);
    let channel = {
      id: "channel",
      name: "General",
      members: [first.pubkey],
      [flag]: true,
    };
    let list = { status: "ready" as const, channels: [channel] };
    const listeners = new Set<() => void>();
    h.retarget({
      session: {
        ...h.session,
        channels: {
          ...h.session.channels,
          get: () => channel,
          list: () => list,
          subscribeList: (listener) => {
            listeners.add(listener);
            return () => {
              listeners.delete(listener);
            };
          },
        },
      },
    });
    fireEvent.keyDown(h.input(), { key: "ArrowUp" });
    expect(h.input()).toHaveValue("");
    act(() => {
      channel = { ...channel, [flag]: false };
      list = { ...list, channels: [channel] };
      for (const listener of listeners) listener();
    });
    fireEvent.keyDown(h.input(), { key: "ArrowUp" });
    h.fill("Changes");
    act(() => {
      channel = { ...channel, [flag]: true };
      list = { ...list, channels: [channel] };
      for (const listener of listeners) listener();
    });
    h.submit();
    expect(h.messages.edit).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Save changes" })).toBeDisabled();
    fireEvent.keyDown(h.input(), { key: "Escape" });
    expect(h.input()).toHaveValue("");
  },
);

it.each([undefined, "thread-root"])(
  "does not edit a diff-only conversation (thread: %s)",
  (threadRootId) => {
    const h = mount({}, undefined, first.pubkey);
    const row = editableMessage({
      diff: { filePath: "a.ts", truncated: false },
      content: "raw patch",
      ...(threadRootId ? { threadRootId } : {}),
    });
    if (threadRootId) h.retarget({ threadRootId, editMessages: [row] });
    else h.setRows([row]);
    fireEvent.keyDown(h.input(), { key: "ArrowUp" });
    expect(h.input()).toHaveValue("");
    expect(screen.queryByText("Editing message")).not.toBeInTheDocument();
    expect(h.messages.edit).not.toHaveBeenCalled();
  },
);

it("uses the full channel choice set for one selected chip and follows membership and policy changes", () => {
  const h = mount();
  const profiles = new Map([
    [first.pubkey, { name: "Honey" }],
    [second.pubkey, { name: "Honey", isAgent: true as const }],
  ]);
  let list = {
    status: "ready" as const,
    channels: [
      {
        id: "channel",
        name: "General",
        members: [first.pubkey, second.pubkey],
      },
    ],
  };
  const listeners = new Set<() => void>();
  let policyChanged = () => {};
  let providers = [createAgentDirectory()];
  const session = {
    ...h.session,
    profiles: { ...h.session.profiles, snapshot: () => profiles },
    channels: {
      ...h.session.channels,
      list: () => list,
      subscribeList: (listener: () => void) => {
        listeners.add(listener);
        return () => {
          listeners.delete(listener);
        };
      },
    },
  };
  const names = bindNames(session, {
    snapshot: () => providers,
    subscribe: (listener) => {
      policyChanged = listener;
      return () => {};
    },
  });
  h.retarget({ session: { ...session, names } });
  act(() => h.commands().insertMention(second));
  const label = () => within(h.input()).getByRole("img").textContent;
  expect(label()).toBe("@Honey (agent)");
  const source = h.input().value;
  act(() => {
    list = {
      ...list,
      channels: [{ id: "channel", name: "General", members: [second.pubkey] }],
    };
    for (const notify of listeners) notify();
  });
  expect(label()).toBe("@Honey");
  act(() => {
    providers = [
      {
        ...createAgentDirectory(),
        scope: () => () => ({ name: "Alternative" }),
      },
    ];
    policyChanged();
  });
  expect(label()).toBe("@Alternative");
  act(() => {
    providers = [];
    policyChanged();
  });
  expect(label()).toBe("@Honey");
  expect(h.input()).toHaveValue(source);
  h.submit();
  expect(h.messages.send).toHaveBeenCalledWith(
    "channel",
    source,
    [second.pubkey],
    [],
  );
  h.unmount();
  names.dispose();
});

it("sends someone outside a DM as a reference without asking", async () => {
  const h = mount();
  const add = vi.fn();
  const list = {
    status: "ready",
    channels: [
      {
        id: "channel",
        channelType: "dm",
        members: ["d".repeat(64), second.pubkey],
        participants: [second.pubkey],
      },
    ],
  };
  Object.assign(h.session, {
    channels: { list: () => list, subscribeList: () => () => {} },
    memberAdditions: { add },
    // A writer that could add members still cannot add anyone to a DM.
    outbox: { ...h.session.outbox, supports: () => true },
  });
  act(() => {
    h.commands().insertMention(first);
    h.commands().insertMention(second);
  });
  fireEvent.submit(screen.getByRole("form"));
  await act(async () => {});
  expect(screen.queryByRole("dialog")).toBeNull();
  expect(h.messages.send).toHaveBeenCalledOnce();
  expect(h.messages.send.mock.calls[0]?.[2]).toEqual([second.pubkey]);
  expect(h.messages.send.mock.calls[0]?.at(-1)).toEqual([first.pubkey]);
  expect(add).not.toHaveBeenCalled();
});

it("does not ask about outside recipients in a session media-comment composer", async () => {
  // Media comments mount the composer with a thread root but without session
  // mode, so the session rule must come from the channel type.
  const h = mount({ threadRootId: "f".repeat(64) });
  const add = vi.fn();
  const channel = {
    id: "channel",
    channelType: "session",
    members: [first.pubkey, second.pubkey],
  };
  const list = { status: "ready", channels: [channel] };
  Object.assign(h.session, {
    channels: { list: () => list, subscribeList: () => () => {} },
    memberAdditions: { add },
    outbox: { ...h.session.outbox, supports: () => true },
  });
  act(() => {
    h.commands().insertMention(first);
    h.commands().insertMention(second);
  });
  // The first person leaves the session after being named.
  channel.members = [second.pubkey];
  fireEvent.submit(screen.getByRole("form"));
  await act(async () => {});
  expect(screen.queryByRole("dialog")).toBeNull();
  expect(h.messages.reply).toHaveBeenCalledOnce();
  expect(h.messages.reply.mock.calls[0]?.[3]).toEqual([
    first.pubkey,
    second.pubkey,
  ]);
  expect(add).not.toHaveBeenCalled();
});

for (const channelType of ["stream", "forum"] as const)
  it.each([undefined, "f".repeat(64)])(
    `keeps mixed nonmember mentions as references after Send anyway in ${channelType}, root=%s`,
    async (threadRootId) => {
      const h = mount(threadRootId ? { threadRootId } : {});
      const add = vi.fn();
      const list = {
        status: "ready",
        channels: [
          {
            id: "channel",
            channelType,
            members: ["d".repeat(64), second.pubkey],
          },
        ],
      };
      Object.assign(h.session, {
        channels: { list: () => list, subscribeList: () => () => {} },
        memberAdditions: { add },
        outbox: { ...h.session.outbox, supports: (kind: number) => kind === 9 },
      });
      act(() => {
        h.commands().insertMention(first);
        h.commands().insertMention(second);
      });
      fireEvent.submit(screen.getByRole("form"));
      expect(screen.getByRole("dialog")).toBeInTheDocument();
      // block/buzz parity: without permission, Invite is absent, not disabled.
      expect(screen.queryByRole("button", { name: "Invite" })).toBeNull();
      await waitFor(() =>
        expect(
          screen.getByRole("button", { name: "Send anyway" }),
        ).toHaveFocus(),
      );
      expect(screen.getByRole("dialog")).toHaveTextContent(
        "Honey is not in this channel. You cannot add people to this channel. You can still send without inviting them.",
      );
      expect(add).not.toHaveBeenCalled();
      fireEvent.click(screen.getByRole("button", { name: "Send anyway" }));
      await act(async () => {});
      const send = threadRootId ? h.messages.reply : h.messages.send;
      expect(send).toHaveBeenCalledOnce();
      expect(send.mock.calls[0]?.[threadRootId ? 3 : 2]).toEqual([
        second.pubkey,
      ]);
      expect(send.mock.calls[0]?.at(-1)).toEqual([first.pubkey]);
      expect(add).not.toHaveBeenCalled();
    },
  );

it.each(["Do nothing", "Invite"])(
  "after %s, the next draft carries an outside agent only if it was notified",
  async (action) => {
    const h = mount();
    h.setProfiles(new Map([[first.pubkey, { name: "Honey", isAgent: true }]]));
    const list = {
      status: "ready",
      channels: [
        { id: "channel", channelType: "stream", members: ["d".repeat(64)] },
      ],
    };
    const add = vi.fn(async (_channel: string, pubkey: string) => {
      list.channels[0]?.members.push(pubkey);
    });
    // A managed agent outside the channel, offered because the viewer can add it.
    const real = h.session.agentChoices;
    const offered = {
      ...real.snapshot(),
      identities: [{ ...first, managed: true }],
    };
    Object.assign(h.session, {
      viewer: "d".repeat(64),
      channels: { list: () => list, subscribeList: () => () => {} },
      memberAdditions: { add },
      agentChoices: { ...real, snapshot: () => offered },
    });
    act(() => {
      h.commands().insertMention(first);
    });
    fireEvent.submit(screen.getByRole("form"));
    fireEvent.click(screen.getByRole("button", { name: action }));
    await waitFor(() => expect(h.messages.send).toHaveBeenCalledOnce());
    await h.user.keyboard("again");
    await h.user.click(screen.getByRole("button", { name: "Send message" }));
    // A reference is not addressed again, so the send needs no second prompt.
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(h.messages.send).toHaveBeenCalledTimes(2);
    expect(h.messages.send.mock.calls[1]?.[2]).toEqual(
      action === "Invite" ? [first.pubkey] : [],
    );
  },
);

it("Enter invites a nonmember before sending", async () => {
  const h = mount();
  const add = vi.fn(async () => {});
  const list = {
    status: "ready",
    channels: [
      { id: "channel", channelType: "stream", members: ["d".repeat(64)] },
    ],
  };
  Object.assign(h.session, {
    viewer: "d".repeat(64),
    channels: { list: () => list, subscribeList: () => () => {} },
    memberAdditions: { add },
  });
  act(() => h.commands().insertMention(first));
  fireEvent.submit(screen.getByRole("form"));
  const invite = screen.getByRole("button", { name: "Invite" });
  await waitFor(() => expect(invite).toHaveFocus());
  await userEvent.setup().keyboard("{Enter}");
  await waitFor(() => expect(h.messages.send).toHaveBeenCalledOnce());
  expect(add).toHaveBeenCalledWith("channel", first.pubkey, undefined, {
    startAgent: false,
  });
  expect(h.messages.send.mock.calls[0]?.[2]).toEqual([first.pubkey]);
});

it.each(["close", "escape"])(
  "%s preserves the captured draft and returns focus without adding or sending",
  async (action) => {
    const h = mount();
    const add = vi.fn();
    const list = {
      status: "ready",
      channels: [
        { id: "channel", channelType: "stream", members: ["d".repeat(64)] },
      ],
    };
    Object.assign(h.session, {
      viewer: "d".repeat(64),
      channels: { list: () => list, subscribeList: () => () => {} },
      memberAdditions: { add },
    });
    act(() => {
      h.commands().insertMention(first);
    });
    const input = h.input();
    fireEvent.submit(screen.getByRole("form"));
    // block/buzz parity: one send action, one invite action, and no Cancel.
    expect(screen.getByRole("dialog")).toHaveTextContent(
      "Honey is not in this channel. Invite them to the channel, or send without inviting them.",
    );
    expect(screen.queryByRole("button", { name: "Cancel" })).toBeNull();
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Invite" })).toHaveFocus(),
    );
    if (action === "close")
      fireEvent.click(screen.getByRole("button", { name: "Close" }));
    else await userEvent.setup().keyboard("{Escape}");
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    expect(input).toHaveValue("@Honey ");
    await waitFor(() => expect(input).toHaveFocus());
    expect(add).not.toHaveBeenCalled();
    expect(h.messages.send).not.toHaveBeenCalled();
  },
);

it.each(["retry", "unmount", "retarget", "disabled", "suspended"])(
  "waits for confirmed addition and handles %s without duplicate sends",
  async (outcome) => {
    const h = mount();
    let release = () => {};
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const list = {
      status: "ready",
      channels: [
        { id: "channel", channelType: "stream", members: ["d".repeat(64)] },
      ],
    };
    const add = vi.fn(async () => {
      await gate;
    });
    if (outcome === "retry")
      add.mockRejectedValueOnce(new Error("Membership not confirmed"));
    Object.assign(h.session, {
      viewer: "d".repeat(64),
      channels: { list: () => list, subscribeList: () => () => {} },
      memberAdditions: { add },
    });
    act(() => {
      h.commands().insertMention(first);
    });
    fireEvent.submit(screen.getByRole("form"));
    fireEvent.click(screen.getByRole("button", { name: "Invite" }));
    try {
      if (outcome === "retry") {
        await screen.findByRole("alert");
        expect(h.messages.send).not.toHaveBeenCalled();
        fireEvent.click(screen.getByRole("button", { name: "Invite" }));
      }
      expect(screen.getByRole("button", { name: "Do nothing" })).toBeDisabled();
      expect(screen.getByRole("button", { name: "Inviting…" })).toBeDisabled();
      if (outcome === "unmount") h.unmount();
      else if (outcome === "retarget") h.retarget({ channelId: "other" });
      else if (outcome === "disabled") h.retarget({ disabled: true });
      else if (outcome === "suspended") {
        h.present(false);
        expect(document.body.querySelector('[role="dialog"]')).toBeNull();
      }
    } finally {
      await act(async () => release());
    }
    if (outcome === "suspended") {
      h.present(true);
      expect(document.body.querySelector('[role="dialog"]')).toBeNull();
      expect(h.input()).toHaveValue("@Honey ");
    }
    expect(add).toHaveBeenCalledTimes(outcome === "retry" ? 2 : 1);
    expect(h.messages.send).toHaveBeenCalledTimes(outcome === "retry" ? 1 : 0);
  },
);

it("disabled completion keeps the highlighted key and consumes Enter without sending", async () => {
  const h = mount();
  h.input().focus();
  h.fill("!Honey");
  const publish = h.completionRequests.at(-1);
  if (!publish) throw new Error("No completion request");
  act(() => {
    publish({
      items: [
        { id: first.pubkey, label: "First Honey", edit: { mention: first } },
        { id: second.pubkey, label: "Second Honey", edit: { mention: second } },
      ],
    });
  });
  fireEvent.keyDown(h.input(), { key: "ArrowDown" });
  act(() => {
    publish({
      items: [
        { id: first.pubkey, label: "First Honey", edit: { mention: first } },
        {
          id: second.pubkey,
          label: "Second Honey",
          edit: { mention: second },
          disabled: "Archived",
        },
      ],
    });
  });
  expect(screen.getByRole("option", { name: "Second Honey" })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  expect(screen.getByRole("option", { name: "Second Honey" })).toHaveAttribute(
    "aria-disabled",
    "true",
  );
  fireEvent.keyDown(h.input(), { key: "Enter" });
  expect(h.input()).toHaveValue("!Honey");
  expect(h.messages.send).not.toHaveBeenCalled();
});

it("rejects a known archived recipient at send entry without clearing the draft", () => {
  const h = mount();
  let archived = false;
  const snapshot = { status: "ready" as const, archived: [] as string[] };
  h.retarget({
    session: {
      ...h.session,
      archives: {
        snapshot: () => snapshot,
        subscribe: () => () => {},
        state: () => (archived ? "archived" : "not-archived"),
        ensure: async () => {},
        refresh: async () => {},
        writable: false,
        consent: vi.fn(),
        request: vi.fn(),
      },
    },
  });
  act(() => {
    expect(h.commands().insertMention(first)).toBe(true);
  });
  archived = true;
  h.submit();
  expect(h.messages.send).not.toHaveBeenCalled();
  expect(h.input()).toHaveValue(`@${first.name} `);
  expect(
    screen.getByText(
      "A selected recipient is archived. Remove it before sending.",
    ),
  ).toBeVisible();
});

it("sends a resource between mentions and keeps both after a failed send", () => {
  const h = mount();
  act(() => {
    const { insertMention, insertResource } = h.commands();
    expect(insertMention(first)).toBe(true);
    expect(insertResource(resource)).toBe(true);
    expect(insertMention(second)).toBe(true);
  });
  h.messages.send.mockImplementationOnce(() => {
    throw new Error("outbox full");
  });
  h.submit();
  expect(h.input()).toHaveTextContent("Resource: Fix login");
  h.submit();
  expect(h.messages.send.mock.calls.at(-1)?.slice(1, 3)).toEqual([
    `@Honey [Fix login](${resource.uri}) @Honey `,
    [first.pubkey, second.pubkey],
  ]);
});

describe("project resource picker", () => {
  const empty: readonly never[] = [];
  const repository = {
    type: "repo",
    owner: resourceOwner,
    dtag: "game",
    address: `30617:${resourceOwner}:game`,
    name: "Game repo",
    description: "",
    event: {} as never,
  } satisfies Entity;
  const project = {
    ...repository,
    type: "project",
    dtag: "proj",
    address: `30621:${resourceOwner}:proj`,
    name: "Proj",
  } satisfies Entity;
  const item = {
    id: resourceRoute.id,
    kind: 1621,
    pubkey: resourceOwner,
    created_at: 5,
    content: "Fix login",
    tags: [
      ["a", repository.address],
      ["subject", "Fix login"],
    ],
  };
  const row = /^Fix login, Issue in Game repo$/;
  function picker(
    home: () => Promise<unknown> = () =>
      Promise.resolve({ status: "home", project }),
    issues: readonly (typeof item)[] = [item],
  ) {
    const h = mount({ extensions: undefined });
    let release: (() => void) | undefined;
    let fail: (() => void) | undefined;
    const validations: AbortSignal[] = [];
    const load = vi.fn(
      (route: { type: string; tab?: string }, signal: AbortSignal) => {
        if (route.type === "project")
          return Promise.resolve({
            items: route.tab === "prs" ? [] : issues,
            repositories: [repository],
            truncated: route.tab === "prs",
          });
        validations.push(signal);
        return new Promise((resolve, reject) => {
          release = () => resolve({});
          fail = () => reject(new Error("offline"));
        });
      },
    );
    const homes = vi.fn(home);
    Object.assign(h.session as object, { projects: { home: homes, load } });
    const tools: readonly Contribution<ComposerTool>[] = [
      {
        id: "resources",
        key: "projects/resources",
        pluginId: "projects",
        revision: "1",
        title: "Issues and pull requests",
        component: ResourcePicker,
      },
    ];
    h.retarget({
      extensions: {
        tools: { snapshot: () => tools, subscribe: () => () => {} },
        inline: { snapshot: () => empty, subscribe: () => () => {} },
        completions: { snapshot: () => empty, subscribe: () => () => {} },
      },
    });
    return {
      h,
      homes,
      validations,
      release: async () => {
        await act(async () => {
          release?.();
        });
      },
      fail: async () => {
        await act(async () => {
          fail?.();
        });
      },
      async open() {
        const trigger = await screen.findByRole("button", {
          name: "Add issue or pull request",
        });
        await waitFor(() => expect(trigger).not.toBeDisabled());
        await h.user.click(trigger);
        return trigger;
      },
    };
  }

  it("validates the chosen row, inserts it, closes and leaves focus in the draft", async () => {
    const p = picker();
    await p.open();
    expect(
      await screen.findByText(/Some issues or pull requests may be missing/),
    ).toBeVisible();
    const choice = await screen.findByRole("button", { name: row });
    expect(choice).toHaveTextContent("Issue · Game repo");
    // Synthetic coverage checks the guard, not native IME behavior.
    const search = screen.getByRole("searchbox");
    for (const composition of [{ isComposing: true }, { keyCode: 229 }]) {
      fireEvent.keyDown(search, { key: "ArrowDown", ...composition });
      expect(search).toHaveFocus();
      // fireEvent returns false when a handler cancelled the default.
      expect(fireEvent.keyDown(search, { key: "Enter", ...composition })).toBe(
        true,
      );
      expect(p.validations).toHaveLength(0);
    }
    // Keyboard: focus stays in search with the row highlighted; Enter
    // chooses it.
    expect(search).toHaveAttribute("aria-activedescendant", choice.id);
    await p.h.user.keyboard("{ArrowDown}");
    expect(search).toHaveFocus();
    expect(choice).toHaveAttribute("data-selected");
    await p.h.user.keyboard("{Enter}");
    expect(p.validations).toHaveLength(1);
    expect(choice).toHaveTextContent("Checking…");
    await p.release();
    expect(screen.queryByRole("button", { name: row })).toBeNull();
    expect(p.h.input()).toHaveTextContent("Resource: Fix login");
    expect(p.h.input()).toHaveFocus();
    p.h.submit();
    expect(p.h.messages.send.mock.calls[0]?.[1]).toBe(
      `[Fix login](${resource.uri}) `,
    );
  });

  it("orders titles by match: prefix, then word start, then the rest", async () => {
    const issue = (id: string, title: string, created_at: number) => ({
      ...item,
      id: id.repeat(64),
      created_at,
      content: title,
      tags: [
        ["a", repository.address],
        ["subject", title],
      ],
    });
    const p = picker(undefined, [
      issue("a", "Relogin bug", 9),
      item,
      issue("b", "Login page", 1),
    ]);
    await p.open();
    const titles = async () =>
      (
        await screen.findAllByRole("button", { name: /, Issue in Game repo$/ })
      ).map((choice) => choice.getAttribute("aria-label")?.split(",")[0]);
    expect(await titles()).toEqual(["Relogin bug", "Fix login", "Login page"]);
    const search = screen.getByRole("searchbox");
    await p.h.user.type(search, "login");
    expect(await titles()).toEqual(["Login page", "Fix login", "Relogin bug"]);
    const choice = screen.getByRole("button", { name: /^Login page,/ });
    expect(search).toHaveAttribute("aria-activedescendant", choice.id);
    expect(choice.querySelector("mark")).toHaveTextContent(/^login$/i);
    // Up wraps to the last row; Shift+Enter is not a choice.
    await p.h.user.keyboard("{ArrowUp}");
    expect(search).toHaveAttribute(
      "aria-activedescendant",
      screen.getByRole("button", { name: /^Relogin bug,/ }).id,
    );
    fireEvent.keyDown(search, { key: "Enter", shiftKey: true });
    expect(p.validations).toHaveLength(0);
    await p.h.user.keyboard("{ArrowDown}{Enter}");
    expect(p.validations).toHaveLength(1);
    expect(choice).toHaveTextContent("Checking…");
  });

  it("highlights and announces the row that Tab focuses", async () => {
    const p = picker(undefined, [
      item,
      {
        ...item,
        id: "b".repeat(64),
        created_at: 1,
        content: "Login page",
        tags: [
          ["a", repository.address],
          ["subject", "Login page"],
        ],
      },
    ]);
    await p.open();
    const second = await screen.findByRole("button", { name: /^Login page,/ });
    const search = screen.getByRole("searchbox");
    const list = document.getElementById(
      search.getAttribute("aria-controls") ?? "",
    );
    expect(list).toContainElement(second);
    for (let i = 0; i < 10 && document.activeElement !== second; i += 1)
      await p.h.user.tab();
    expect(second).toHaveFocus();
    expect(second).toHaveAttribute("data-selected");
    expect(screen.getByRole("button", { name: row })).not.toHaveAttribute(
      "data-selected",
    );
    expect(search).toHaveAttribute("aria-activedescendant", second.id);
    expect(
      screen.getByText("Login page. Press Enter to add.", { exact: true }),
    ).toBeInTheDocument();
  });

  it("puts an exact title before a newer title that starts with it", async () => {
    const p = picker(undefined, [
      {
        ...item,
        id: "b".repeat(64),
        created_at: item.created_at + 1,
        content: "Fix login page",
        tags: [
          ["a", repository.address],
          ["subject", "Fix login page"],
        ],
      },
      item,
    ]);
    await p.open();
    const search = screen.getByRole("searchbox");
    await p.h.user.type(search, "fix login");
    const choice = screen.getByRole("button", { name: row });
    expect(
      (
        await screen.findAllByRole("button", { name: /, Issue in Game repo$/ })
      ).map((option) => option.getAttribute("aria-label")?.split(",")[0]),
    ).toEqual(["Fix login", "Fix login page"]);
    expect(search).toHaveAttribute("aria-activedescendant", choice.id);
    await p.h.user.keyboard("{Enter}");
    expect(p.validations).toHaveLength(1);
    expect(choice).toHaveTextContent("Checking…");
  });

  it("keeps focus on the highlighted row after Tab, for arrows and pointer", async () => {
    const p = picker(undefined, [
      item,
      {
        ...item,
        id: "b".repeat(64),
        created_at: 1,
        content: "Login page",
        tags: [
          ["a", repository.address],
          ["subject", "Login page"],
        ],
      },
    ]);
    await p.open();
    const first = await screen.findByRole("button", { name: row });
    const second = screen.getByRole("button", { name: /^Login page,/ });
    for (let i = 0; i < 10 && document.activeElement !== first; i += 1)
      await p.h.user.tab();
    expect(first).toHaveFocus();
    // Down on a focused row moves the highlight and focus together.
    await p.h.user.keyboard("{ArrowDown}");
    expect(second).toHaveFocus();
    expect(second).toHaveAttribute("data-selected");
    expect(first).not.toHaveAttribute("data-selected");
    // A pointer highlight moves focus too, so Enter adds the highlighted row.
    fireEvent.pointerMove(first, { clientX: 1, clientY: 1 });
    fireEvent.pointerMove(first, { clientX: 2, clientY: 2 });
    expect(first).toHaveAttribute("data-selected");
    expect(first).toHaveFocus();
    await p.h.user.keyboard("{Enter}");
    expect(p.validations).toHaveLength(1);
    expect(first).toHaveTextContent("Checking…");
  });

  it("keeps focus in the popover while a clicked row is checked", async () => {
    const p = picker();
    await p.open();
    await p.h.user.click(await screen.findByRole("button", { name: row }));
    expect(screen.getByRole("button", { name: row })).toBeDisabled();
    expect(screen.getByRole("searchbox")).toHaveFocus();
    expect(screen.getByText("Checking the chosen item…")).toHaveAttribute(
      "role",
      "status",
    );
    await p.fail();
    expect(screen.getByRole("alert")).toHaveTextContent(
      "Could not check this item",
    );
    expect(screen.getByRole("searchbox")).toHaveFocus();
    expect(screen.queryByText("Checking the chosen item…")).toBeNull();
  });

  it("keeps a rejected insertion in the popover with the host reason", async () => {
    const p = picker();
    p.h.fill("`ab`");
    p.h.input().setSelectionRange(2, 2);
    await p.open();
    await p.h.user.click(await screen.findByRole("button", { name: row }));
    await p.release();
    expect(screen.getByRole("alert")).toHaveTextContent(
      "Links can't be added inside code or other Markdown here",
    );
    expect(screen.getByRole("button", { name: row })).toBeVisible();
    expect(p.h.input()).toHaveValue("`ab`");
  });

  it("drops a pending choice when the composer is disabled and re-enabled", async () => {
    const p = picker();
    await p.open();
    await p.h.user.click(await screen.findByRole("button", { name: row }));
    p.h.retarget({ disabled: true });
    p.h.retarget({ disabled: false });
    expect(p.validations[0]?.aborted).toBe(true);
    expect(screen.queryByRole("button", { name: row })).toBeNull();
    await p.release();
    expect(p.h.input()).toHaveValue("");
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("drops a pending choice on send instead of landing it in the next draft", async () => {
    const p = picker();
    p.h.fill("first message");
    await p.open();
    await p.h.user.click(await screen.findByRole("button", { name: row }));
    await p.h.user.click(screen.getByRole("button", { name: /^Send/ }));
    expect(p.h.messages.send.mock.calls[0]?.[1]).toBe("first message");
    expect(p.validations[0]?.aborted).toBe(true);
    await p.release();
    expect(p.h.input()).toHaveValue("");
    expect(screen.queryByRole("button", { name: row })).toBeNull();
  });

  it("hides only for no project, and explains ambiguity or failure with a retry", async () => {
    const none = picker(() => Promise.resolve({ status: "none" }));
    await waitFor(() => expect(none.homes).toHaveBeenCalled());
    await act(async () => {});
    expect(
      screen.queryByRole("button", { name: "Add issue or pull request" }),
    ).toBeNull();
    cleanup();
    let resolveHome: ((value: unknown) => void) | undefined;
    const ambiguous = picker(() =>
      resolveHome
        ? new Promise((resolve) => {
            resolveHome = resolve;
          })
        : Promise.resolve({ status: "ambiguous" }),
    );
    await ambiguous.open();
    expect(
      await screen.findByText(/belongs to more than one project/),
    ).toBeVisible();
    expect(screen.queryByRole("button", { name: row })).toBeNull();
    // Ambiguity is recoverable: once the conflict is resolved, retry rereads.
    resolveHome = () => {};
    await ambiguous.h.user.click(
      screen.getByRole("button", { name: "Retry project" }),
    );
    expect(await screen.findByText("Loading project…")).toBeInTheDocument();
    await act(async () => {
      resolveHome?.({ status: "home", project });
    });
    expect(await screen.findByRole("button", { name: row })).toBeVisible();
    expect(screen.queryByText(/belongs to more than one project/)).toBeNull();
    cleanup();
    const failed = picker(() => Promise.reject(new Error("offline")));
    await failed.open();
    const retry = await screen.findByRole("button", { name: "Retry project" });
    const reads = failed.homes.mock.calls.length;
    await failed.h.user.click(retry);
    await waitFor(() =>
      expect(failed.homes.mock.calls.length).toBeGreaterThan(reads),
    );
  });
});

it("keeps a composed message unchanged through caret keys at its end and refuses a Right Arrow committed as text in either form", async () => {
  const h = mount();
  await h.user.type(h.input(), "Hello!");
  await h.user.keyboard("{Shift>}{Enter}{/Shift}world");
  expect(h.input()).toHaveValue("Hello!\nworld");
  const html = h.input().innerHTML;
  for (let i = 0; i < 3; i++) await h.user.keyboard("{ArrowRight}");
  for (const key of [
    "ArrowLeft",
    "ArrowUp",
    "ArrowDown",
    "Shift",
    "Meta",
    "Escape",
  ])
    await h.user.keyboard(`{${key}}`);
  // jsdom does not model Home and End on a contenteditable element.
  for (const key of ["Home", "End"]) {
    fireEvent.keyDown(h.input(), { key, code: key });
    fireEvent.keyUp(h.input(), { key, code: key });
  }
  expect(h.input()).toHaveValue("Hello!\nworld");
  expect(h.input().innerHTML).toBe(html);
  expect(h.messages.send).not.toHaveBeenCalled();
  // The desktop build committed Right Arrow's raw keyboard-layout translation
  // U+001D; AppKit's function-key character for the key is U+F703. Neither
  // has a glyph, so each assertion names its form rather than the character.
  for (const [label, character] of [
    ["Right Arrow's layout translation U+001D", "\u001D"],
    ["Right Arrow's function-key character U+F703", "\uF703"],
  ] as const) {
    let prevented = false;
    act(() => {
      h.input().focus();
      prevented = !h.input().dispatchEvent(
        new InputEvent("beforeinput", {
          bubbles: true,
          cancelable: true,
          inputType: "insertText",
          data: character,
        }),
      );
    });
    expect(prevented, label).toBe(true);
    expect(h.input(), label).toHaveValue("Hello!\nworld");
    expect(h.input().innerHTML, label).toBe(html);
  }
  // The keydown such a press arrives as: `key` is the control character while
  // `code` and the legacy key code still name Right Arrow. It is claimed before
  // the native path can type it, and neither sends nor edits the message.
  let prevented = false;
  act(() => {
    prevented = !fireEvent.keyDown(h.input(), {
      key: "\u001D",
      code: "ArrowRight",
      keyCode: 39,
    });
  });
  expect(prevented).toBe(true);
  expect(h.input()).toHaveValue("Hello!\nworld");
  expect(h.input().innerHTML).toBe(html);
  expect(h.messages.send).not.toHaveBeenCalled();
});

it("retires composer transients and revoked insert commands without replacing the editor or draft", async () => {
  const h = mount();
  h.fill("retained draft");
  const editor = h.input();
  const commands = h.commands();
  fireEvent.click(screen.getByRole("button", { name: "Toggle formatting" }));
  fireEvent.click(screen.getByRole("button", { name: /^Link/ }));
  expect(screen.getByRole("dialog")).toBeInTheDocument();
  await waitFor(() =>
    expect(screen.getByRole("dialog")).toContainElement(
      document.activeElement as HTMLElement,
    ),
  );
  const focus = vi.spyOn(editor, "focus");
  h.present(false);
  expect(document.body.querySelector('[role="dialog"]')).toBeNull();
  expect(
    screen.queryByRole("button", { name: "First Honey", hidden: true }),
  ).toBeNull();
  act(() => {
    expect(commands.insertText("late picker selection")).toBe(false);
  });
  h.present(true);
  act(() => {
    expect(commands.insertText("retired command after recovery")).toBe(false);
  });
  expect(h.input()).toBe(editor);
  expect(editor).toHaveValue("retained draft");
  expect(document.body.querySelector('[role="dialog"]')).toBeNull();
  await act(() => Promise.resolve());
  expect(focus).not.toHaveBeenCalled();
  expect(h.messages.send).not.toHaveBeenCalled();
});

it("adds an HTTP link in the dialog and reports unsafe destinations accurately", () => {
  const h = mount();
  h.fill("docs");
  const editor = h.input();
  act(() => editor.setSelectionRange(0, 4));
  fireEvent.click(screen.getByRole("button", { name: "Toggle formatting" }));
  fireEvent.click(screen.getByRole("button", { name: /^Link/ }));
  expect(screen.getByRole("dialog", { name: "Add link" })).toBeInTheDocument();
  fireEvent.change(screen.getByRole("textbox", { name: "URL" }), {
    target: { value: "http://user:pw@example.com" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Save" }));
  expect(screen.getByRole("alert")).toHaveTextContent(
    "Use an HTTP(S) URL or a valid buzz:// link.",
  );
  fireEvent.change(screen.getByRole("textbox", { name: "URL" }), {
    target: { value: "http://example.com/docs" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Save" }));
  expect(screen.queryByRole("dialog")).toBeNull();
  expect(editor).toHaveValue("docs");
  expect(h.input().innerHTML).toContain("http://example.com/docs");
});

it("restores the retained editor when the Link dialog closes normally", async () => {
  const h = mount();
  const editor = h.input();
  fireEvent.click(screen.getByRole("button", { name: "Toggle formatting" }));
  fireEvent.click(screen.getByRole("button", { name: /^Link/ }));
  await waitFor(() =>
    expect(screen.getByRole("dialog")).toContainElement(
      document.activeElement as HTMLElement,
    ),
  );
  fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  await waitFor(() => expect(editor).toHaveFocus());
  expect(document.body.querySelector('[role="dialog"]')).toBeNull();
});

it("keeps a local attachment and its editor alive while presentation is suspended", async () => {
  const h = mount();
  let finish!: (value: {
    name: string;
    url: string;
    type: string;
    size: number;
    sha256: string;
  }) => void;
  const gate = new Promise<{
    name: string;
    url: string;
    type: string;
    size: number;
    sha256: string;
  }>((resolve) => {
    finish = resolve;
  });
  let signal!: AbortSignal;
  const upload = vi.fn(
    (_file: File, _channel: string, current: AbortSignal) => {
      signal = current;
      return gate;
    },
  );
  h.retarget({ session: { ...h.session, attachments: { upload } } });
  const editor = h.input();
  h.fill("kept with upload");
  fireEvent.change(screen.getByLabelText("Choose attachments"), {
    target: { files: [new NodeFile(["notes"], "notes.txt")] },
  });
  h.present(false);
  expect(upload).not.toHaveBeenCalled();
  expect(editor.isConnected).toBe(true);
  expect(editor).toHaveValue("kept with upload");

  h.present(true);
  expect(h.input()).toBe(editor);
  fireEvent.submit(
    screen.getByRole("form", { name: "Send a message to General" }),
  );
  await waitFor(() => expect(upload).toHaveBeenCalledOnce());
  try {
    expect(signal.aborted).toBe(false);
  } finally {
    await act(async () =>
      finish({
        name: "notes.txt",
        url: "https://relay.test/media/notes.txt",
        type: "text/plain",
        size: 5,
        sha256: "a".repeat(64),
      }),
    );
  }
  expect(h.messages.send).toHaveBeenCalledExactlyOnceWith(
    "channel",
    "kept with upload",
    [],
    [
      {
        name: "notes.txt",
        url: "https://relay.test/media/notes.txt",
        type: "text/plain",
        size: 5,
        sha256: "a".repeat(64),
      },
    ],
  );
  expect(signal.aborted).toBe(false);
  expect(upload).toHaveBeenCalledOnce();
});

it("dismisses completion observations without reopening them on recovery", async () => {
  const h = mount();
  act(() => h.input().focus());
  h.fill("!query");
  h.publish(h.completionRequests.length - 1, "Completed choice");
  expect(screen.getByText("Completed choice")).toBeInTheDocument();
  const editor = h.input();
  h.present(false);
  expect(document.body.textContent).not.toContain("Completed choice");
  h.present(true);
  expect(h.input()).toBe(editor);
  expect(editor).toHaveValue("!query");
  expect(document.body.textContent).not.toContain("Completed choice");
});

it.each([undefined, "root"])(
  "notifies draft retirement only after accepted %s cleanup persists, retrying without Send",
  async (threadRootId) => {
    const retired = vi.fn();
    const h = mount({
      ...(threadRootId ? { threadRootId } : {}),
      onDraftSaved: retired,
    });
    h.fill("Accepted once");
    const key = threadRootId
      ? `draft:channel:thread:${threadRootId}`
      : "draft:channel";
    const stored = `buzz-view.v1:${JSON.stringify(["scope", key])}`;
    const setItem = Storage.prototype.setItem;
    const fail = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation(function (this: Storage, key, value) {
        if (key === stored) throw Error("no space");
        setItem.call(this, key, value);
      });
    try {
      await h.user.click(screen.getByRole("button", { name: "Send message" }));
      expect(h.onSend).toHaveBeenCalledOnce();
      expect(retired).not.toHaveBeenCalled();
      expect(h.input()).toHaveValue("");
      expect(
        screen.getByRole("button", { name: "Send message" }),
      ).toBeDisabled();
      fireEvent.submit(screen.getByRole("form"));
      expect(
        threadRootId ? h.messages.reply : h.messages.send,
      ).toHaveBeenCalledOnce();
    } finally {
      fail.mockRestore();
    }
    await h.user.click(
      screen.getByRole("button", { name: "Retry draft cleanup" }),
    );
    expect(retired).toHaveBeenCalledExactlyOnceWith();
    expect(h.onSend).toHaveBeenCalledOnce();
    expect(readView("scope", key, "")).toMatchObject({ text: "" });
  },
);

it.each([false, true])(
  "allows successive sends with no saved revision while writes fail (remembered agent: %s)",
  async (remembered) => {
    const retired = vi.fn();
    const h = mount({ onDraftSaved: retired });
    h.setProfiles(new Map([[first.pubkey, { name: "Honey", isAgent: true }]]));
    const key = 'buzz-view.v1:["scope","draft:channel"]';
    const original = Storage.prototype.setItem;
    const fail = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation(function (this: Storage, name, value) {
        if (name === key) throw Error("full");
        original.call(this, name, value);
      });
    try {
      h.fill("First message");
      if (remembered)
        await h.user.click(screen.getByRole("button", { name: "First Honey" }));
      await h.user.click(screen.getByRole("button", { name: "Send message" }));
      expect(h.messages.send).toHaveBeenCalledOnce();
      expect(h.input()).toHaveValue(remembered ? "@Honey " : "");
      expect(h.input()).not.toHaveAttribute("contenteditable", "false");
      expect(
        screen.queryByRole("button", { name: "Retry draft cleanup" }),
      ).not.toBeInTheDocument();
      expect(localStorage.getItem(key)).toBeNull();
      expect(retired).toHaveBeenCalledTimes(remembered ? 0 : 1);
      if (remembered) {
        expect(screen.getByRole("alert")).toHaveTextContent(
          "Could not save this draft",
        );
        expect(
          screen.getByRole("button", { name: "Send message" }),
        ).toBeEnabled();
        // Keep the actual inline recipient while appending new work.
        await h.user.keyboard("Second message");
      } else h.fill("Second message");
      await h.user.click(screen.getByRole("button", { name: "Send message" }));
      expect(h.messages.send).toHaveBeenCalledTimes(2);
      expect(h.messages.send.mock.calls[1]?.[1]).toContain("Second message");
      expect(h.messages.send.mock.calls[1]?.[2]).toEqual(
        remembered ? [first.pubkey] : [],
      );
      expect(localStorage.getItem(key)).toBeNull();
      expect(retired).toHaveBeenCalledTimes(remembered ? 0 : 2);
      // Failed absence-only cleanup must not leave a session recovery lock.
      h.retarget({ channelId: "other" });
      h.retarget({ channelId: "channel" });
      h.fill("After remount");
      expect(
        screen.getByRole("button", { name: "Send message" }),
      ).toBeEnabled();
      expect(
        screen.queryByRole("button", { name: "Retry draft cleanup" }),
      ).not.toBeInTheDocument();
    } finally {
      fail.mockRestore();
    }
  },
);

it.each([false, true])(
  "still saves the replacement of an absent draft when writes recover (remembered: %s)",
  async (remembered) => {
    const retired = vi.fn();
    const h = mount({ onDraftSaved: retired });
    h.setProfiles(new Map([[first.pubkey, { name: "Honey", isAgent: true }]]));
    const fail = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation(() => {
        throw Error("full");
      });
    try {
      h.fill("Previously unsaved text");
      if (remembered)
        await h.user.click(screen.getByRole("button", { name: "First Honey" }));
      expect(
        localStorage.getItem('buzz-view.v1:["scope","draft:channel"]'),
      ).toBeNull();
      h.messages.send.mockImplementationOnce(() => {
        fail.mockRestore();
        return "accepted";
      });
      await h.user.click(screen.getByRole("button", { name: "Send message" }));
      expect(readView("scope", "draft:channel", "missing")).toMatchObject({
        text: remembered ? "@Honey " : "",
      });
      expect(retired).toHaveBeenCalledExactlyOnceWith();
      expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    } finally {
      fail.mockRestore();
    }
  },
);

it.each(["captured", "cleanup"])(
  "retains accepted recovery when the %s saved revision is unreadable",
  async (unreadable) => {
    const key = 'buzz-view.v1:["scope","draft:channel"]';
    let blocked = unreadable === "captured";
    const getItem = Storage.prototype.getItem;
    const read = vi
      .spyOn(Storage.prototype, "getItem")
      .mockImplementation(function (this: Storage, name) {
        if (name === key && blocked) throw Error("unreadable");
        return getItem.call(this, name);
      });
    const write = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation(() => {
        throw Error("full");
      });
    try {
      const retired = vi.fn();
      const h = mount({ onDraftSaved: retired });
      h.fill("Accepted but storage cannot be verified");
      h.messages.send.mockImplementationOnce(() => {
        blocked = true;
        return "accepted";
      });
      await h.user.click(screen.getByRole("button", { name: "Send message" }));
      expect(h.messages.send).toHaveBeenCalledOnce();
      expect(retired).not.toHaveBeenCalled();
      expect(
        screen.getByRole("button", { name: "Send message" }),
      ).toBeDisabled();
      await h.user.click(
        screen.getByRole("button", { name: "Retry draft cleanup" }),
      );
      expect(
        screen.getByRole("button", { name: "Retry draft cleanup" }),
      ).toBeVisible();
      expect(h.messages.send).toHaveBeenCalledOnce();
    } finally {
      read.mockRestore();
      write.mockRestore();
    }
  },
);

it.each([false, true])(
  "preserves a newer saved revision when accepted cleanup races a replacement (event: %s)",
  async (notify) => {
    const retired = vi.fn();
    const h = mount({ onDraftSaved: retired });
    h.fill("Send this body");
    h.messages.send.mockImplementationOnce(() => {
      const key = 'buzz-view.v1:["scope","draft:channel"]';
      localStorage.setItem(key, JSON.stringify("Newer saved work"));
      if (notify)
        window.dispatchEvent(
          new StorageEvent("storage", { storageArea: localStorage, key }),
        );
      return "accepted";
    });
    await h.user.click(screen.getByRole("button", { name: "Send message" }));
    expect(h.messages.send).toHaveBeenCalledOnce();
    expect(h.onSend).toHaveBeenCalledExactlyOnceWith("accepted");
    expect(retired).not.toHaveBeenCalled();
    expect(readView("scope", "draft:channel", "")).toBe("Newer saved work");
    expect(h.input()).toHaveValue("Newer saved work");
  },
);

it("compares before local edits and Send even if the storage event has not arrived", async () => {
  const h = mount();
  h.fill("Local body");
  localStorage.setItem(
    'buzz-view.v1:["scope","draft:channel"]',
    JSON.stringify("Saved elsewhere"),
  );
  await h.user.click(screen.getByRole("button", { name: "Send message" }));
  expect(h.messages.send).not.toHaveBeenCalled();
  expect(screen.getByRole("alert")).toHaveTextContent(/changed elsewhere/);
  h.fill("Continue local edits");
  expect(readView("scope", "draft:channel", "")).toBe("Saved elsewhere");
  await h.user.click(screen.getByRole("button", { name: "Load saved draft" }));
  expect(h.input()).toHaveValue("Saved elsewhere");
});

it("keeps the original draft after synchronous outbox rejection without a persistence callback", async () => {
  const retired = vi.fn();
  const h = mount({ onDraftSaved: retired });
  h.fill("Rejected body");
  h.messages.send.mockImplementationOnce(() => {
    throw Error("outbox full");
  });
  await h.user.click(screen.getByRole("button", { name: "Send message" }));
  expect(h.onSend).not.toHaveBeenCalled();
  expect(retired).not.toHaveBeenCalled();
  expect(h.input()).toHaveValue("Rejected body");
  expect(readView("scope", "draft:channel", "")).toMatchObject({
    text: "Rejected body",
  });
  expect(screen.getByRole("button", { name: "Send message" })).toBeEnabled();
});

it("does not restore accepted cleanup over a replacement made while the composer was closed", async () => {
  const h = mount();
  h.fill("Already accepted");
  const fail = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw Error("full");
  });
  try {
    await h.user.click(screen.getByRole("button", { name: "Send message" }));
    expect(
      screen.getByRole("button", { name: "Retry draft cleanup" }),
    ).toBeVisible();
  } finally {
    fail.mockRestore();
  }
  const session = h.session;
  h.unmount();
  writeView("scope", "draft:channel", "A replacement draft");
  const reopened = mount({ session });
  expect(reopened.input()).toHaveValue("A replacement draft");
  expect(
    screen.queryByRole("button", { name: "Retry draft cleanup" }),
  ).not.toBeInTheDocument();
});

it("leaves active message-edit content alone and reconciles the draft on return", () => {
  const h = mount({}, undefined, first.pubkey);
  h.setRows([editableMessage()]);
  fireEvent.keyDown(h.input(), { key: "ArrowUp" });
  h.fill("An edit, not a draft");
  act(() => writeView("scope", "draft:channel", "Draft saved elsewhere"));
  expect(h.input()).toHaveValue("An edit, not a draft");
  expect(readView("scope", "draft:channel", "")).toBe("Draft saved elsewhere");
  fireEvent.click(screen.getByRole("button", { name: "Cancel edit" }));
  expect(h.input()).toHaveValue("Draft saved elsewhere");
  expect(h.messages.send).not.toHaveBeenCalled();
});

it("does not apply ordinary draft reconciliation to submission-owned recovery", () => {
  const submit = vi.fn();
  const h = mount({
    submission: {
      draftKey: "session-retry",
      recoveredDraft: { text: "Durable operation", recipients: [] },
      locked: true,
      disabled: false,
      submit,
    },
  });
  act(() => writeView("scope", "session-retry", "Disposable view state"));
  expect(h.input()).toHaveValue("Durable operation");
  fireEvent.click(screen.getByRole("button", { name: "Send message" }));
  expect(submit).toHaveBeenCalledExactlyOnceWith({
    text: "Durable operation",
    recipients: [],
  });
  expect(
    screen.queryByRole("button", { name: "Load saved draft" }),
  ).not.toBeInTheDocument();
});

it("rechecks the saved revision after held session admission before submitting", async () => {
  const h = mount();
  const channel = {
    id: "channel",
    channelType: "session",
    members: [first.pubkey],
  };
  const list = { status: "ready", channels: [channel] };
  const library = { status: "ready", identities: [first] };
  let release = () => {};
  const refreshMembership = vi.fn(
    () =>
      new Promise<typeof channel>((resolve) => {
        release = () => resolve(channel);
      }),
  );
  h.retarget({
    sessionConversation: true,
    session: {
      ...h.session,
      channels: { list: () => list, subscribeList: () => () => {} },
      agentLibrary: {
        snapshot: () => library,
        subscribe: () => () => {},
      },
      workSessions: { refreshMembership, addAgents: vi.fn() },
    } as unknown as RelaySession,
  });
  await h.user.click(screen.getByRole("button", { name: "First Honey" }));
  h.submit();
  await waitFor(() => expect(refreshMembership).toHaveBeenCalledOnce());
  try {
    expect(screen.getByRole("button", { name: "Send message" })).toBeDisabled();
    // Event delivery can lag a synchronous read in another page.
    localStorage.setItem(
      'buzz-view.v1:["scope","draft:channel"]',
      JSON.stringify("New work during admission"),
    );
  } finally {
    await act(async () => release());
  }
  expect(h.messages.send).not.toHaveBeenCalled();
  expect(screen.getByRole("alert")).toHaveTextContent("changed elsewhere");
  expect(readView("scope", "draft:channel", "")).toBe(
    "New work during admission",
  );
});

it("locks a nonempty remembered-agent follow-up until accepted draft cleanup succeeds", async () => {
  const retired = vi.fn();
  const h = mount({ onDraftSaved: retired });
  vi.spyOn(h.session.profiles, "snapshot").mockReturnValue(
    new Map([[first.pubkey, { name: "Honey", isAgent: true }]]),
  );
  await h.user.click(screen.getByRole("button", { name: "First Honey" }));
  const fail = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw Error("full");
  });
  try {
    await h.user.click(screen.getByRole("button", { name: "Send message" }));
    expect(h.input()).toHaveValue("@Honey ");
    expect(screen.getByRole("button", { name: "Send message" })).toBeDisabled();
    fireEvent.submit(screen.getByRole("form"));
    expect(h.messages.send).toHaveBeenCalledOnce();
    expect(retired).not.toHaveBeenCalled();
  } finally {
    fail.mockRestore();
  }
  await h.user.click(
    screen.getByRole("button", { name: "Retry draft cleanup" }),
  );
  expect(readView("scope", "draft:channel", "")).toMatchObject({
    text: "@Honey ",
  });
  expect(retired).toHaveBeenCalledOnce();
  expect(h.messages.send).toHaveBeenCalledOnce();
});

it("batch insertion deduplicates exact keys and sends individual recipients", async () => {
  const h = mount();
  act(() =>
    expect(h.commands().insertMentions([first, second, first])).toBe(true),
  );
  expect(h.input()).toHaveValue("@Honey @Honey ");
  h.submit();
  await waitFor(() => expect(h.messages.send).toHaveBeenCalled());
  expect(h.messages.send.mock.calls[0]?.[2]).toEqual([
    first.pubkey,
    second.pubkey,
  ]);
});
it("batch insertion rejects an unavailable member atomically and does not send on rejected completion", async () => {
  const h = mount({ inviteAgents: true });
  const unavailable = { pubkey: "c".repeat(64), name: "Missing" };
  h.fill("!Court");
  act(() => {
    h.completionRequests.at(-1)?.({
      items: [
        {
          id: "team",
          label: "Court",
          edit: { mentions: [first, unavailable] },
        },
      ],
    });
  });
  await h.user.keyboard("{Enter}");
  expect(h.input()).toHaveValue("!Court");
  expect(h.messages.send).not.toHaveBeenCalled();
  expect(screen.getByRole("alert")).toHaveTextContent("no longer available");
});
it.each(["disabled", "retarget", "unmount"])(
  "revokes batch commands after %s",
  (transition) => {
    const h = mount();
    const insert = h.commands().insertMentions;
    if (transition === "disabled") h.retarget({ disabled: true });
    else if (transition === "retarget") h.retarget({ channelId: "other" });
    else h.unmount();
    act(() => expect(insert([first, second])).toBe(false));
    expect(h.messages.send).not.toHaveBeenCalled();
  },
);

it("rechecks session setup before publishing after a background upload", async () => {
  const channelId = "22222222-2222-4222-8222-222222222222";
  const h = mount({ channelId });
  const gate = deferred<ReturnType<typeof uploadDescriptor>>();
  const upload = vi.fn(() => gate.promise);
  h.rerender(
    <ToastProvider>
      <MessageComposer
        session={{ ...h.session, attachments: { upload } }}
        scope="scope"
        channelId={channelId}
        channelName="General"
      />
    </ToastProvider>,
  );
  h.fill("Keep this draft");
  fireEvent.change(screen.getByLabelText("Choose attachments"), {
    target: { files: [new NodeFile(["notes"], "notes.txt")] },
  });
  fireEvent.submit(
    screen.getByRole("form", { name: "Send a message to General" }),
  );
  await waitFor(() => expect(upload).toHaveBeenCalledOnce());
  writeView("scope", "sessions:section:work:pending", {
    id: channelId,
    text: "Original start",
    creationId: "c".repeat(64),
    setup: { sectionId: "work", canvas: "Frozen", agents: [] },
  });
  await act(async () => gate.resolve(uploadDescriptor()));
  await waitFor(() => expect(h.input()).toHaveValue("Keep this draft"));
  expect(h.messages.send).not.toHaveBeenCalled();
  expect(h.messages.reply).not.toHaveBeenCalled();
  expect(within(screen.getByRole("form")).getByText("notes.txt")).toBeVisible();
});

it("keeps snapshot paste from taking the attachment slot reserved for a recording", async () => {
  const empty: readonly never[] = [];
  const tools: readonly Contribution<ComposerTool>[] = [
    {
      id: "capture",
      key: "test/capture",
      pluginId: "test",
      revision: "1",
      title: "Capture",
      component: ({ capture }) => (
        <button
          type="button"
          onClick={() =>
            capture?.(({ accept }) => (
              <button
                type="button"
                onClick={() =>
                  accept({
                    file: new File([new Uint8Array(100)], "voice-note.wav", {
                      type: "audio/wav",
                    }),
                    duration: 1,
                    waveform: [0.5],
                  })
                }
              >
                Finish recording
              </button>
            ))
          }
        >
          Start recording
        </button>
      ),
    },
  ];
  const h = await mountUploadComposer({
    media: (url) => `buzz-media://localhost/${encodeURIComponent(url)}`,
    extensions: {
      tools: { snapshot: () => tools, subscribe: () => () => {} },
      inline: { snapshot: () => empty, subscribe: () => () => {} },
      completions: { snapshot: () => empty, subscribe: () => () => {} },
    },
  });
  const picker =
    h.container.querySelector<HTMLInputElement>('input[type="file"]');
  if (!picker) throw new Error("Missing file picker");
  fireEvent.change(picker, {
    target: {
      files: Array.from(
        { length: 9 },
        (_, i) => new File(["draft"], `draft-${i}.txt`, { type: "text/plain" }),
      ),
    },
  });
  fireEvent.click(screen.getByRole("button", { name: "Start recording" }));
  const finish = screen.getByRole("button", { name: "Finish recording" });
  const descriptor = {
    name: "helper.agent.png",
    url: `https://relay.example.test/media/${"a".repeat(64)}.png`,
    type: "image/png",
    size: 2048,
    sha256: "a".repeat(64),
  };
  fireEvent.paste(finish, {
    clipboardData: {
      items: [],
      getData: (type: string) =>
        type === "text/html"
          ? snapshotClipboardHtml(descriptor, "Helper")
          : descriptor.url,
    },
  });
  expect(
    screen.queryByRole("button", { name: "Remove helper.agent.png" }),
  ).toBeNull();
  fireEvent.click(finish);
  expect(
    await screen.findByRole("button", { name: "Remove voice-note.wav" }),
  ).toBeVisible();
  expect(h.uploadCalls).toHaveLength(0);
  expect(h.publish).not.toHaveBeenCalled();
});

it.each(["agent", "team"])(
  "pastes a copied %s snapshot as an attachment and sends only on Send",
  async (kind) => {
    const h = await mountUploadComposer({
      media: (url) => `buzz-media://localhost/${encodeURIComponent(url)}`,
    });
    const descriptor = {
      name: `helper.${kind}.png`,
      url: `https://relay.example.test/media/${"a".repeat(64)}.png`,
      type: "image/png",
      size: 2048,
      sha256: "a".repeat(64),
    };
    const html = snapshotClipboardHtml(descriptor, "Helper");
    fireEvent.paste(h.input(), {
      clipboardData: {
        items: [],
        getData: (type: string) =>
          type === "text/html" ? html : descriptor.url,
      },
    });
    expect(
      await within(h.form()).findByRole("button", {
        name: `Remove ${descriptor.name}`,
      }),
    ).toBeVisible();
    expect(h.input()).toHaveValue("");
    expect(h.publish).not.toHaveBeenCalled();
    expect(h.uploadCalls).toHaveLength(0);
    fireEvent.click(h.send());
    await waitFor(() => expect(h.publish).toHaveBeenCalledTimes(1));
    expect(h.uploadCalls).toHaveLength(0);
    const event = h.publish.mock.calls[0]?.[0];
    expect(event?.content).toContain(descriptor.url);
    expect(event?.tags).toContainEqual(
      expect.arrayContaining([
        "imeta",
        `filename ${descriptor.name}`,
        `x ${descriptor.sha256}`,
      ]),
    );
  },
);

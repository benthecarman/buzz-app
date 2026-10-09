import { npubEncode } from "nostr-tools/nip19";
import { expect, it } from "vitest";
import type { EventData } from "../../features/relay/events";
import { steerPrompt, systemPrompt, timerPrompt, turnPrompt } from "./prompt";

const alice = "a".repeat(64);
const bob = "b".repeat(64);
const agent = "c".repeat(64);
const event = (
  id: string,
  content: string,
  extra: Partial<EventData> = {},
): EventData =>
  ({
    id: id.repeat(64).slice(0, 64),
    pubkey: alice,
    kind: 9,
    created_at: 1_800_000_000,
    content,
    tags: [["h", "chan"]],
    sig: "",
    ...extra,
  }) as EventData;
const names: Record<string, string> = {
  [alice]: "Alice Smith",
  [agent]: "Claude",
};
const name = (pubkey: string) => names[pubkey];

it("builds the harness system prompt with instructions and core memory", () => {
  const prompt = systemPrompt({
    scope: "thread",
    cwd: "~/.buzz",
    instructions: "Be terse.",
    memory: "I am Claude.",
  });
  expect(prompt).toMatch(/^<base>\n/);
  expect(prompt).toContain("## Session Model");
  expect(prompt).toContain(
    "<workspace>\nCurrent working directory: ~/.buzz\n</workspace>",
  );
  expect(prompt).toContain(
    "<agent-instructions>\nBe terse.\n</agent-instructions>",
  );
  expect(prompt).toContain("<core-memory>\nI am Claude.\n</core-memory>");
  expect(systemPrompt({ scope: "thread", cwd: "/w", memory: null })).toContain(
    "No core memory found.",
  );
  expect(systemPrompt({ scope: "channel", cwd: "/w" })).not.toContain(
    "<core-memory>",
  );
});

it("frames a new top-level mention as the root of its thread", () => {
  const mention = event("1", "@Claude what's up?", {
    tags: [
      ["h", "chan"],
      ["p", agent],
    ],
  });
  const prompt = turnPrompt({
    event: mention,
    channel: { id: "chan", name: "general" },
    scope: "thread",
    context: [],
    total: 1,
    label: "@mention",
    name,
  });
  expect(prompt).toContain(
    "Scope: thread\nSession scope: thread\nChannel: general (#chan)",
  );
  expect(prompt).toContain(`Thread root: ${mention.id}`);
  expect(prompt).toContain(
    "This is a new top-level message. `send` replies in its thread by default",
  );
  expect(prompt).not.toContain("<thread-context");
  expect(prompt).toContain('<buzz-event type="@mention">');
  expect(prompt).toContain(
    `From: Alice Smith (npub: ${npubEncode(alice)}, hex: ${alice})`,
  );
  expect(prompt).toContain("Content: @Claude what's up?");
  expect(prompt).toContain(`Parsed: mentions=[Claude (${agent})]`);
});

it("includes the thread messages the session has not seen", () => {
  const root = event("1", "Can someone look at the build?");
  const earlier = event("2", "It fails on main", {
    pubkey: bob,
    created_at: 1_800_000_010,
  });
  const reply = event("3", "@Claude please help", {
    created_at: 1_800_000_020,
    tags: [
      ["h", "chan"],
      ["e", root.id, "", "root"],
      ["e", earlier.id, "", "reply"],
    ],
  });
  const prompt = turnPrompt({
    event: reply,
    channel: { id: "chan" },
    scope: "thread",
    thread: { rootId: root.id, parentId: earlier.id },
    context: [root, earlier],
    total: 3,
    label: "@mention",
    name,
  });
  expect(prompt).toContain(`Thread root: ${root.id}\nParent: ${earlier.id}`);
  expect(prompt).toContain(
    "`send` replies in this thread by default, so the conversation stays threaded",
  );
  expect(prompt).toContain(
    '<thread-context included="2" total="3" truncated="false">',
  );
  expect(prompt).toContain(
    `[1] Alice Smith (${alice}) (2027-01-15T08:00:00.000Z): Can someone look at the build?`,
  );
  expect(prompt).toContain(
    `[2] ${bob} (2027-01-15T08:00:10.000Z): It fails on main`,
  );
  expect(prompt).toContain(`Parsed: parent=${earlier.id}, root=${root.id}`);
  expect(prompt).toMatch(/<\/buzz-event>$/);
});

it("says earlier context is already in the session when none is new", () => {
  const prompt = turnPrompt({
    event: event("3", "and another thing"),
    channel: { id: "dm", dm: true },
    scope: "thread",
    context: [],
    total: 5,
    label: "@mention",
    name,
  });
  expect(prompt).toContain("Scope: dm\nSession scope: dm conversation");
  expect(prompt).toContain(
    "Earlier context is already available in this session.",
  );
  // A DM's session spans its threads, so it names the one to reply in.
  expect(prompt).toContain(
    `pass \`reply: "${event("3", "").id}"\` to \`send\``,
  );
});

it("shows a DM's recent messages as conversation context", () => {
  const prompt = turnPrompt({
    event: event("3", "and this"),
    channel: { id: "dm", dm: true },
    scope: "thread",
    context: [event("1", "hi")],
    total: 2,
    label: "@mention",
    name,
  });
  expect(prompt).toContain('<conversation-context included="1" total="2"');
});

it("wraps a message that arrives mid-turn and frames a timer", () => {
  expect(steerPrompt("<buzz-event>x</buzz-event>")).toMatch(
    /^<new-message-arrived-while-you-were-working>\n<buzz-event>x<\/buzz-event>\n<\/new-message-arrived-while-you-were-working>\n\nNote: A new message arrived/,
  );
  const timer = timerPrompt({
    id: "daily",
    interest: "ops",
    prompt: "Summarize",
    instructions: "Be brief",
    spent: false,
  });
  expect(timer).toContain("Timer: daily");
  expect(timer).toContain("Prompt: Summarize");
  expect(timer).not.toContain("spent: true");
  expect(timer).toContain('<interest id="ops">\nBe brief\n</interest>');
  // A timer whose Interest is gone says so, rather than running without one.
  expect(
    timerPrompt({ id: "daily", interest: "o<p", prompt: "x", spent: true }),
  ).toContain(
    '<interest id="o&lt;p" missing="true">\nThis timer names Interest "o&lt;p", which does not exist.',
  );
});

it("adds attention guidance only for a session with attention tools", () => {
  expect(
    systemPrompt({ scope: "thread", cwd: "/w", attention: true }),
  ).toContain("## Attention");
  expect(systemPrompt({ scope: "thread", cwd: "/w" })).not.toContain(
    "## Attention",
  );
});

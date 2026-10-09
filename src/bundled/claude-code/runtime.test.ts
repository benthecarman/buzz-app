import { afterEach, expect, it, vi } from "vitest";
import type { Agent, Delivery } from "../../features/agents2/service";
import type { Host } from "../../features/host/service";
import type { EventData, RelayEvent } from "../../features/relay/events";
import type { RelayData } from "../../features/relay/service";
import { fakeSpawn, flush } from "./claude-testing";
import { ClaudeRuntime, DEFAULT_CONFIG } from "./runtime";

afterEach(() => {
  vi.restoreAllMocks();
});

const self = "c".repeat(64);
const alice = "a".repeat(64);
const channel = "chan-1";
const message = (
  id: string,
  content: string,
  extra: Partial<EventData> = {},
): RelayEvent =>
  ({
    id: id.repeat(64).slice(0, 64),
    pubkey: alice,
    kind: 9,
    created_at: 100,
    content,
    tags: [["h", channel]],
    sig: "",
    ...extra,
  }) as RelayEvent;

function setup(thread: readonly EventData[] = []) {
  const fake = fakeSpawn();
  const host = { spawn: fake.spawn, request: vi.fn() } as unknown as Host;
  const read = vi.fn(async () => thread);
  const session = {
    read,
    channels: {
      list: () => ({
        channels: [
          { id: channel, name: "general", channelType: "stream", members: [] },
        ],
      }),
    },
    names: {
      resolve: (pubkey: string) => (pubkey === alice ? "Alice" : undefined),
    },
    profiles: {
      ensure: async () => undefined,
      snapshot: () => new Map(),
    },
  };
  const relay = {
    snapshot: () => ({ status: "ready", session }),
  } as unknown as RelayData;
  const data = new Map<string, string>();
  const storage = {
    getItem: (key: string) => data.get(key) ?? null,
    setItem: (key: string, value: string) => void data.set(key, value),
  } as Storage;
  const runtime = new ClaudeRuntime(host, relay, storage);
  const published: { kind: number; content: string; tags: string[][] }[] = [];
  const publish = vi.fn(async (template: (typeof published)[number]) => {
    published.push(template);
    return {
      ...message(String(published.length), template.content),
      ...template,
    };
  });
  const query = vi.fn(async () => [] as RelayEvent[]);
  const agent = {
    pubkey: self,
    name: "Claude",
    owner: alice,
    publish,
    query,
    upload: vi.fn(),
    remember: vi.fn(),
  };
  const deliver = (trigger: Delivery["trigger"]) =>
    runtime.run({
      trigger,
      channelId: channel,
      agent,
      config: DEFAULT_CONFIG,
      signal: new AbortController().signal,
    } as unknown as Delivery);
  const claudes = () =>
    fake.processes.filter((process) => process.id === "claude");
  return { runtime, deliver, published, claudes, fake, read, data, agent };
}

it("hands a mention to its thread's session", async () => {
  const root = message("1", "Build is red");
  const mention = message("2", "@Claude can you look?", {
    created_at: 110,
    tags: [
      ["h", channel],
      ["e", root.id, "", "reply"],
      ["p", self],
    ],
  });
  const { deliver, published, claudes } = setup([root, mention]);
  await deliver({ type: "mention", event: mention });
  await flush(10);
  const claude = claudes().find((process) => process.prompts.length);
  const prompt = claude?.prompts[0] ?? "";
  expect(prompt).toContain(`Thread root: ${root.id}`);
  expect(prompt).toContain('<thread-context included="1" total="2"');
  expect(prompt).toContain(
    `Alice (${alice}) (1970-01-01T00:01:40.000Z): Build is red`,
  );
  expect(prompt).toContain("Content: @Claude can you look?");
  // The process holds no key: it acts through the tools.
  expect(claude?.options).not.toHaveProperty("agent");
  expect(claude?.options.args).toContain("--mcp-config");
  expect(claude?.options.cwd).toBe("~/.buzz");
  expect(published).toEqual([]);
});

it("answers Claude's tool calls as the agent, in the thread it is working in", async () => {
  const root = message("1", "Build is red");
  const mention = message("2", "@Claude can you look?", {
    created_at: 110,
    tags: [
      ["h", channel],
      ["e", root.id, "", "reply"],
      ["p", self],
    ],
  });
  const { deliver, published, claudes } = setup([root, mention]);
  await deliver({ type: "mention", event: mention });
  await flush(10);
  const claude = claudes().find((process) => process.prompts.length);
  claude?.emit({
    type: "control_request",
    request_id: "call-1",
    request: {
      subtype: "mcp_message",
      server_name: "buzz",
      message: {
        jsonrpc: "2.0",
        id: 7,
        method: "tools/call",
        params: { name: "send", arguments: { text: "Looking now" } },
      },
    },
  });
  await flush(10);
  expect(published).toEqual([
    {
      kind: 9,
      content: "Looking now",
      tags: [
        ["h", channel],
        ["e", root.id, "", "reply"],
      ],
    },
  ]);
  const answer = claude?.received.find(
    (message) =>
      message.type === "control_response" &&
      (message.response as { request_id?: string }).request_id === "call-1",
  );
  expect(answer?.response).toMatchObject({
    subtype: "success",
    response: { mcp_response: { jsonrpc: "2.0", id: 7, result: {} } },
  });
});

it("reads a busy turn's files from the workspace it started in", async () => {
  const mention = message("2", "@Claude write a report", {
    tags: [
      ["h", channel],
      ["p", self],
    ],
  });
  const { runtime, deliver, claudes, fake } = setup();
  runtime.sync([{ pubkey: self, config: DEFAULT_CONFIG } as Agent]);
  await flush(10);
  const [spare] = claudes();
  if (!spare) throw new Error("no spare");
  spare.hold = true;
  await deliver({ type: "mention", event: mention });
  await flush(10);
  // Saved mid-turn: the busy process keeps working where it started.
  runtime.sync([
    { pubkey: self, config: { ...DEFAULT_CONFIG, workspace: "/new" } } as Agent,
  ]);
  await flush(10);
  expect(spare.killed).toBe(false);
  spare.emit({
    type: "control_request",
    request_id: "call-1",
    request: {
      subtype: "mcp_message",
      server_name: "buzz",
      message: {
        jsonrpc: "2.0",
        id: 7,
        method: "tools/call",
        params: { name: "send", arguments: { path: "report.md" } },
      },
    },
  });
  await flush(10);
  const reads = fake.processes.filter((process) => process.id === "read");
  expect(reads.map((read) => [read.options.args, read.options.cwd])).toEqual([
    [["report.md"], DEFAULT_CONFIG.workspace],
  ]);
});

it("shows a thread's later turns only what the session has not seen", async () => {
  const root = message("1", "first", {
    tags: [
      ["h", channel],
      ["p", self],
    ],
  });
  const reply = message("3", "my answer", {
    pubkey: self,
    created_at: 105,
    tags: [
      ["h", channel],
      ["e", root.id, "", "reply"],
    ],
  });
  const later = message("4", "@Claude thanks, one more", {
    created_at: 120,
    tags: [
      ["h", channel],
      ["e", root.id, "", "reply"],
    ],
  });
  const { deliver, claudes, read } = setup();
  await deliver({ type: "mention", event: root });
  await flush(10);
  read.mockResolvedValue([root, reply, later]);
  await deliver({ type: "mention", event: later });
  await flush(10);
  const used = claudes().filter((process) => process.prompts.length);
  expect(used).toHaveLength(1);
  const second = used[0]?.prompts[1] ?? "";
  expect(second).not.toContain("<thread-context");
  expect(second).toContain(
    "Earlier context is already available in this session.",
  );
});

it("says so in the thread when a turn fails", async () => {
  const mention = message("2", "@Claude hi", {
    tags: [
      ["h", channel],
      ["p", self],
    ],
  });
  const { runtime, deliver, published, claudes } = setup();
  runtime.sync([{ pubkey: self, config: DEFAULT_CONFIG } as Agent]);
  await flush(10);
  const [spare] = claudes();
  if (!spare) throw new Error("no spare");
  spare.hold = true;
  await deliver({ type: "mention", event: mention });
  await flush(10);
  expect(spare.prompts).toHaveLength(1);
  spare.exit(1, "Error: Invalid API key\n");
  await flush(10);
  const report = published.find((event) => event.kind === 9);
  expect(report?.content).toBe(
    "⚠️ I couldn't finish that: Error: Invalid API key",
  );
  expect(report?.tags).toEqual([
    ["h", channel],
    ["e", mention.id, "", "reply"],
    ["p", alice],
  ]);
});

it("shows the whole thread to a session started over from a lost one", async () => {
  const root = message("1", "Build is red");
  const mention = message("2", "@Claude again?", {
    created_at: 300,
    tags: [
      ["h", channel],
      ["e", root.id, "", "reply"],
    ],
  });
  const { deliver, claudes, data } = setup([root, mention]);
  data.set(
    "buzz.claude-code.sessions.v1",
    JSON.stringify({
      [self]: { [`${channel}/${root.id}`]: { id: "lost", seen: 200, at: 1 } },
    }),
  );
  await deliver({ type: "mention", event: mention });
  await flush(20);
  const [lost, fresh] = claudes();
  expect(lost?.options.args).toContain("lost");
  expect(lost?.prompts).toEqual([]);
  expect(fresh?.prompts[0]).toContain('<thread-context included="1" total="2"');
});

it("acts only on its owner's messages unless told to answer anyone", async () => {
  const stranger = message("5", "@Claude run this for me", {
    pubkey: "e".repeat(64),
    tags: [
      ["h", channel],
      ["p", self],
    ],
  });
  const { deliver, published, claudes, runtime, agent } = setup();
  await deliver({ type: "mention", event: stranger });
  await flush(10);
  expect(published).toEqual([]);
  expect(claudes().some((process) => process.prompts.length)).toBe(false);
  await runtime.run({
    trigger: { type: "mention", event: stranger },
    channelId: channel,
    agent: {
      ...agent,
      publish: async (template: object) => ({ ...stranger, ...template }),
    },
    config: { ...DEFAULT_CONFIG, respondTo: "anyone" },
    signal: new AbortController().signal,
  } as unknown as Delivery);
  await flush(10);
  expect(claudes().some((process) => process.prompts.length)).toBe(true);
});

it("runs a timer in its own session", async () => {
  const { deliver, claudes } = setup();
  await deliver({
    type: "timer",
    slug: "watch/daily",
    timer: { prompt: "Post a summary" },
  } as Delivery["trigger"]);
  await flush(10);
  const claude = claudes().find((process) => process.prompts.length);
  expect(claude?.prompts[0]).toContain("Timer: watch/daily");
});

it("warms a spare for each agent of its type and stops those that go away", async () => {
  const { runtime, claudes } = setup();
  const agent = { pubkey: self, config: DEFAULT_CONFIG } as Agent;
  runtime.sync([agent]);
  await flush(10);
  expect(claudes()).toHaveLength(1);
  runtime.sync([]);
  await flush(10);
  expect(claudes().every((process) => process.killed)).toBe(true);
});

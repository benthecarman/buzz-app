// Turns Agents2 deliveries into Claude turns. Each agent of this type has its
// own sessions; a delivery becomes a prompt in the harness format, sent to the
// session for its conversation; a watch or timer wake runs as a one-off turn in
// a new session, in its Interest's lane. The run returns once the turn is handed over,
// so one agent works in several conversations at once. Claude acts in Buzz with
// the in-process Buzz tools, signed by the agent's native-held key; the runtime
// only reports a turn that fails.
import type { Context, Memory } from "../../buzz-mcp/client";
import { respond } from "../../buzz-mcp/rpc";
import type {
  Agent,
  AgentHandle,
  Delivery,
} from "../../features/agents2/service";
import type { Host } from "../../features/host/service";
import type { EventData } from "../../features/relay/events";
import type { RelayData } from "../../features/relay/service";
import { threadReference } from "../../features/relay/thread-reference";
import type { Spawn, ToolServer } from "./claude";
import { appClient } from "./client";
import { Lanes } from "./lanes";
import {
  ATTENTION_REVIEW,
  CONTEXT_LIMIT,
  type Scope,
  steerPrompt,
  systemPrompt,
  timerPrompt,
  turnPrompt,
  watchPrompt,
} from "./prompt";
import { AgentSessions, localSessions } from "./sessions";

export type Config = Readonly<{
  /** A Claude model or alias; empty uses Claude Code's own default. */
  model: string;
  instructions: string;
  /** The agent's working directory, absolute or `~/…`. */
  workspace: string;
  /** Whether each thread is its own session, or the whole channel is one. */
  scope: Scope;
  /** Whose messages it acts on. Claude runs commands as you, so by default
   * only yours, as harness agents do. */
  respondTo: "owner" | "anyone";
}>;
export const DEFAULT_CONFIG: Config = Object.freeze({
  model: "",
  instructions: "",
  workspace: "~/.buzz",
  scope: "thread",
  respondTo: "owner",
});
export const config = (value: unknown): Config => ({
  ...DEFAULT_CONFIG,
  ...(value && typeof value === "object" ? (value as Partial<Config>) : {}),
});

/** How long a read of the agent's core memory is reused. */
const MEMORY_TTL_MS = 5 * 60_000;
const MEMORY_TIMEOUT_MS = 5_000;
const NAMES_TIMEOUT_MS = 2_000;
const CHAT = [9, 40002];

type Entry = {
  sessions: AgentSessions;
  config: Config;
  /** Whether the owner has attention on: new sessions get its tools, its
   * guidance and the end-of-turn review. */
  attention: boolean;
  /** Watch and timer turns, one lane per Interest. */
  lanes: Lanes;
  memory?: { value: string | null | undefined; at: number };
  /** The agent's handle from its latest delivery, for the tools to act with. */
  handle?: AgentHandle;
  /** Where each conversation's tools default to: its latest turn's thread. */
  contexts: Map<string, Context>;
};

export class ClaudeRuntime {
  private readonly agents = new Map<string, Entry>();
  private readonly reading = new Map<string, Promise<readonly Memory[]>>();
  private readonly listeners = new Set<() => void>();
  /** Each agent's last session list, kept until something changes so a view
   * reading it gets the same array back. */
  private readonly views = new Map<
    string,
    ReturnType<AgentSessions["snapshot"]>
  >();

  constructor(
    private readonly host: Host,
    private readonly relay: RelayData,
    private readonly storage: Storage = globalThis.localStorage,
  ) {}

  private get spawn(): Spawn {
    const spawn = this.host.spawn?.bind(this.host);
    if (!spawn)
      throw new Error("Claude Code agents run only in the desktop app");
    return spawn;
  }

  /** Brings the runtime in line with this type's agents: new ones get a warm
   * session, changed settings restart idle sessions, removed agents stop. */
  sync(agents: readonly Agent[]) {
    const current = new Set(agents.map((agent) => agent.pubkey));
    // Out of view (another community, offline) or deleted: either way its
    // processes stop. Saved sessions stay so it resumes when it is back.
    for (const [pubkey, entry] of this.agents)
      if (!current.has(pubkey)) {
        entry.lanes.stop();
        entry.sessions.dispose();
        this.agents.delete(pubkey);
      }
    for (const agent of agents) {
      const next = config(agent.config);
      const entry = this.agents.get(agent.pubkey);
      if (!entry)
        this.entry(agent.pubkey, next, agent.attentionEnabled).sessions.warm();
      else if (
        JSON.stringify(entry.config) !== JSON.stringify(next) ||
        entry.attention !== agent.attentionEnabled
      ) {
        // Either changes what a new session is told and offered. Turned off,
        // its waiting watch and timer turns are dropped too.
        if (!agent.attentionEnabled) entry.lanes.clear();
        entry.config = next;
        entry.attention = agent.attentionEnabled;
        entry.sessions.reconfigure();
      }
    }
    this.notify();
  }

  /** Conversations each agent has running, for its status view. */
  sessions(pubkey: string) {
    let view = this.views.get(pubkey);
    if (!view) {
      view = this.agents.get(pubkey)?.sessions.snapshot() ?? [];
      this.views.set(pubkey, view);
    }
    return view;
  }
  subscribe(listener: () => void) {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }
  private notify() {
    this.views.clear();
    for (const listener of this.listeners) listener();
  }

  dispose() {
    for (const entry of this.agents.values()) {
      entry.lanes.stop();
      entry.sessions.dispose();
    }
    this.agents.clear();
  }

  async run({ trigger, agent, config: raw, channelId }: Delivery) {
    const settings = config(raw);
    // `respondTo` is whose requests it acts on. A watch is the agent's own
    // attention: its event is observed data, framed as such, from anyone.
    if (
      trigger.type === "mention" &&
      settings.respondTo !== "anyone" &&
      trigger.event.pubkey !== agent.owner
    )
      return;
    const entry = this.entry(agent.pubkey, settings, agent.attention.enabled());
    entry.handle = agent;
    if (trigger.type !== "mention") {
      this.wake(entry, agent, trigger);
      return;
    }
    const { event } = trigger;
    const channel =
      channelId ?? event.tags.find((tag) => tag[0] === "h")?.[1] ?? undefined;
    if (!channel) return;
    // `fresh` is for a session that has been shown nothing yet.
    const prompt = async (fresh: boolean) => {
      const turn = await this.turn(entry, agent.pubkey, event, channel, fresh);
      return {
        key: turn.key,
        text: this.review(entry, turnPrompt({ ...turn, label: "@mention" })),
      };
    };
    const { key, text } = await prompt(false);
    // A conversation's tools default to its thread; a session shared by
    // several threads is told which to reply in instead.
    entry.contexts.set(
      key,
      key === channel
        ? { channel }
        : { channel, root: threadReference(event)?.rootId ?? event.id },
    );
    const done = entry.sessions.deliver(key, text, event.created_at, {
      steer: steerPrompt(text),
      fresh: async () => (await prompt(true)).text,
    });
    this.notify();
    this.watch(done, async (error) => {
      if (error) await this.report(agent, channel, event, error);
    });
  }

  /** A watch or timer wake: a one-off turn in its Interest's lane, as in
   * Janet. It joins no conversation, and nothing about it is saved. Its prompt
   * is built when the turn starts, from the attention as it is then: a turn
   * whose watch was disabled or removed, or whose agent's attention went off,
   * while it waited does not run. */
  private wake(
    entry: Entry,
    agent: AgentHandle,
    trigger: Exclude<Delivery["trigger"], { type: "mention" }>,
  ) {
    const { slug } = trigger;
    const id = slug.replace(/^watch\//, "");
    const lane = (trigger.type === "watch" ? trigger.watch : trigger.timer)
      .interest_id;
    const queued = entry.lanes.add(lane, async () => {
      const text = await this.wakePrompt(agent, trigger, id);
      if (!text) return;
      const done = entry.sessions.runOnce(`${trigger.type}/${id}`, text);
      this.notify();
      const result = await done;
      this.notify();
      if (!result.ok)
        console.warn(
          `Claude Code ${trigger.type} turn failed: ${id}`,
          result.error,
        );
    });
    if (queued !== "queued")
      console.warn(
        queued === "full"
          ? `Claude Code dropped a ${trigger.type} turn: too many are waiting (Interest ${lane})`
          : `Claude Code dropped a ${trigger.type} turn: the agent stopped`,
      );
  }

  /** The prompt for a wake as its watch now is, or undefined when it should
   * no longer run. */
  private async wakePrompt(
    agent: AgentHandle,
    trigger: Exclude<Delivery["trigger"], { type: "mention" }>,
    id: string,
  ) {
    const { attention } = agent;
    if (!attention.enabled()) return undefined;
    const current = (await attention.show(trigger.slug)).object;
    const type = trigger.type === "watch" ? "event" : "timer";
    if (!current || current.type !== type || !current.enabled) return undefined;
    const interest = (await attention.show(`interest/${current.interest_id}`))
      .object;
    const instructions =
      interest?.type === "interest"
        ? { instructions: interest.instructions }
        : {};
    const text =
      trigger.type === "watch"
        ? watchPrompt({
            id,
            interest: current.interest_id,
            ...instructions,
            event: trigger.event,
            ...(trigger.classifier ? { classifier: trigger.classifier } : {}),
          })
        : timerPrompt({
            id,
            interest: current.interest_id,
            prompt: current.type === "timer" ? current.prompt : "",
            ...instructions,
            spent: trigger.spent,
          });
    // Read again: the switch can change while the attention is read.
    return attention.enabled() ? `${text}\n\n${ATTENTION_REVIEW}` : undefined;
  }

  /** `text` with Janet's end-of-turn attention review, when attention is on. */
  private review(entry: Entry, text: string) {
    return entry.attention ? `${text}\n\n${ATTENTION_REVIEW}` : text;
  }

  private watch(
    done: Promise<{ ok: boolean; error?: string }>,
    after: (error?: string) => unknown,
  ) {
    void done
      .then((result) => after(result.ok ? undefined : result.error))
      .catch((error) => console.warn("Claude Code turn failed", error))
      .finally(() => this.notify());
  }

  /** Says in the thread that a turn failed, so nobody waits on it. */
  private report(
    agent: AgentHandle,
    channel: string,
    event: EventData,
    error: string,
  ) {
    const root = threadReference(event)?.rootId ?? event.id;
    return agent
      .publish({
        kind: 9,
        content: `⚠️ I couldn't finish that: ${error.slice(0, 500)}`,
        tags: [
          ["h", channel],
          ...(root === event.id ? [] : [["e", root, "", "root"]]),
          ["e", event.id, "", "reply"],
          ["p", event.pubkey],
        ],
      })
      .catch((failure) =>
        console.warn("Claude Code could not report a failed turn", failure),
      );
  }

  /** Where the event's turn goes, and what its prompt needs. */
  private async turn(
    entry: Entry,
    self: string,
    event: EventData,
    channelId: string,
    fresh = false,
  ) {
    const snapshot = this.relay.snapshot();
    const session = snapshot.status === "ready" ? snapshot.session : undefined;
    const summary =
      session?.channels.list().channels.find((item) => item.id === channelId) ??
      session?.channels.get?.(channelId);
    const dm = summary?.channelType === "dm";
    const thread = threadReference(event);
    const rootId = thread?.rootId ?? event.id;
    const scope = entry.config.scope;
    const key =
      dm || scope === "channel" ? channelId : `${channelId}/${rootId}`;
    const seen = fresh ? 0 : entry.sessions.seen(key);
    let earlier: readonly EventData[] = [];
    if (session && (thread || dm))
      try {
        const events = await session.read(
          thread
            ? [
                { ids: [rootId], limit: 1 },
                { kinds: CHAT, "#h": [channelId], "#e": [rootId], limit: 200 },
              ]
            : [{ kinds: CHAT, "#h": [channelId], limit: CONTEXT_LIMIT + 1 }],
          { signal: AbortSignal.timeout(NAMES_TIMEOUT_MS * 2) },
        );
        earlier = events
          .filter(
            (item) =>
              CHAT.includes(item.kind) &&
              item.id !== event.id &&
              item.created_at <= event.created_at,
          )
          .sort((a, b) => a.created_at - b.created_at);
      } catch (error) {
        console.warn("Claude Code could not read the conversation", error);
      }
    // A session already has its own replies; a new one is shown them too.
    const unseen = earlier.filter(
      (item) => item.created_at > seen && (!seen || item.pubkey !== self),
    );
    const pubkeys = [
      ...new Set([
        event.pubkey,
        ...unseen.map((item) => item.pubkey),
        ...event.tags
          .filter((tag) => tag[0] === "p")
          .map((tag) => tag[1] ?? ""),
      ]),
    ].filter(Boolean);
    if (session)
      await Promise.race([
        session.profiles.ensure(pubkeys, "foreground").catch(() => undefined),
        new Promise((resolve) => setTimeout(resolve, NAMES_TIMEOUT_MS)),
      ]);
    const members = summary?.members ?? [];
    return {
      key,
      event,
      scope,
      channel: {
        id: channelId,
        ...(summary?.name ? { name: summary.name } : {}),
        dm,
      },
      ...(thread ? { thread } : {}),
      context: unseen,
      total: earlier.length + 1,
      name: (pubkey: string) =>
        session?.names.resolve(pubkey, undefined, members) ??
        session?.profiles.snapshot().get(pubkey)?.name ??
        undefined,
    };
  }

  private entry(pubkey: string, initial: Config, attention: boolean): Entry {
    const existing = this.agents.get(pubkey);
    if (existing) return existing;
    const entry: Entry = {
      config: initial,
      attention,
      lanes: new Lanes(),
      sessions: undefined as unknown as AgentSessions,
      contexts: new Map(),
    };
    // Each process reads files where it was started, and keeps the tool list
    // it was given, until it is replaced.
    const tools =
      (cwd: string, attention: boolean): ToolServer =>
      async (conversation, message) => {
        // A spare starts before any delivery: it can list the tools, and has a
        // handle to call them with by the time it is given a turn.
        const handle = entry.handle;
        const client = handle && {
          ...appClient(handle, {
            spawn: this.spawn,
            cwd,
            memories: () => this.memories(pubkey),
          }),
          remember: async (slug: string, body: string, after: number) => {
            await handle.remember(slug, body, after);
            if (slug === "core") delete entry.memory;
          },
        };
        const context =
          (conversation && entry.contexts.get(conversation)) || {};
        return respond(client, context, message, {
          offered: attention,
          ...(handle ? { api: handle.attention } : {}),
        });
      };
    entry.sessions = new AgentSessions({
      spawn: (id, options) => this.spawn(id, options),
      store: localSessions(this.storage, pubkey),
      onChange: () => this.notify(),
      fingerprint: () =>
        JSON.stringify([
          entry.config.model,
          entry.config.workspace,
          entry.config.instructions,
          entry.config.scope,
          entry.attention,
        ]),
      launch: async () => {
        const { model, workspace, instructions, scope } = entry.config;
        const { attention } = entry;
        const memory = await this.memory(pubkey, entry);
        return {
          cwd: workspace,
          tools: tools(workspace, attention),
          ...(model.trim() ? { model: model.trim() } : {}),
          systemPrompt: systemPrompt({
            scope,
            cwd: workspace,
            instructions,
            attention,
            ...(memory === undefined ? {} : { memory }),
          }),
        };
      },
    });
    this.agents.set(pubkey, entry);
    return entry;
  }

  /** The agent's `core` memory, as the harness injects it into a new session:
   * its text, `null` when it has none, or undefined when it could not be read. */
  private async memory(pubkey: string, entry: Entry) {
    if (entry.memory && Date.now() - entry.memory.at < MEMORY_TTL_MS)
      return entry.memory.value;
    let value: string | null | undefined;
    try {
      const memories = await this.memories(pubkey);
      value = memories.find((memory) => memory.slug === "core")?.body ?? null;
    } catch (error) {
      console.warn("Claude Code could not read the agent's memory", error);
    }
    entry.memory = { value, at: Date.now() };
    return value;
  }

  /** The agent's memory, read back through the owner's session: it is
   * encrypted to the owner, so no agent key is needed to read it. */
  private memories(pubkey: string): Promise<readonly Memory[]> {
    // The owner's session holds few memory views at once, so calls share one.
    let reading = this.reading.get(pubkey);
    if (!reading) {
      reading = this.readMemories(pubkey).finally(() =>
        this.reading.delete(pubkey),
      );
      this.reading.set(pubkey, reading);
    }
    return reading;
  }
  private async readMemories(pubkey: string): Promise<readonly Memory[]> {
    const snapshot = this.relay.snapshot();
    if (snapshot.status !== "ready") throw new Error("Buzz is not connected");
    const view = snapshot.session.agentMemories.open(pubkey);
    try {
      const done = new Promise<void>((resolve) => {
        const settled = () =>
          !["idle", "loading"].includes(view.snapshot().status) && resolve();
        view.subscribe(settled);
        settled();
      });
      void view.refresh();
      await Promise.race([
        done,
        new Promise((_, reject) =>
          setTimeout(
            () => reject(new Error("Memory read timed out")),
            MEMORY_TIMEOUT_MS,
          ),
        ),
      ]);
      const { status, listing } = view.snapshot();
      if (status !== "ready" || !listing)
        throw new Error(`Memory unavailable (${status})`);
      return listing.entries;
    } finally {
      view.dispose();
    }
  }
}

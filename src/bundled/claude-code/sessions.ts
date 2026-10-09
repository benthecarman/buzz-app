// One agent's Claude sessions: one per conversation (a thread, or a whole DM),
// each continuing across messages and app restarts. A conversation in use keeps
// its process; a quiet one is stopped and resumed from its saved session when it
// is next addressed. A spare process waits for the next new conversation, so
// that starts without a spawn. A one-off turn (a watch or timer wake) gets a
// new session of its own, which is never saved, resumed or shared.
import {
  ClaudeProcess,
  type ClaudeLaunch,
  type Settled,
  type Spawn,
} from "./claude";

/** Processes an agent keeps, the spare included. */
export const MAX_LIVE = 6;
/** A conversation's process stops after this long without a message. */
export const IDLE_MS = 15 * 60_000;
/** How long a one-off turn waits for a free process, and then how long it
 * can run, before it is given up; Janet's turn deadline. */
export const ONCE_DEADLINE_MS = 5 * 60_000;
const ROOM_POLL_MS = 1_000;
/** Saved sessions per agent; the least recently used are forgotten. */
const SAVED_LIMIT = 200;

/** What is saved for one conversation. */
export type SavedSession = Readonly<{
  id: string;
  /** created_at of the newest event this session has been shown. */
  seen: number;
  at: number;
}>;
export type SessionStore = {
  get(key: string): SavedSession | undefined;
  set(key: string, session: SavedSession): void;
  delete(key: string): void;
};

export type SessionsOptions = Readonly<{
  spawn: Spawn;
  store: SessionStore;
  /** How to start a process with the agent's current settings. */
  launch(): Promise<Omit<ClaudeLaunch, "resume" | "sessionId">>;
  /** Changes when a setting that a running process fixed has changed. */
  fingerprint(): string;
  now?(): number;
  newId?(): string;
  /** Called when a conversation's process starts or stops. */
  onChange?(): void;
}>;

type Live = {
  process: ClaudeProcess;
  fingerprint: string;
  used: number;
  timer?: ReturnType<typeof setTimeout>;
};

export class AgentSessions {
  private readonly live = new Map<string, Live>();
  /** One-off turns running or starting, by label. They count toward
   * MAX_LIVE, and are never stopped to make room. */
  private readonly once = new Map<string, Live | undefined>();
  private onceCount = 0;
  private readonly opening = new Map<string, Promise<Live | undefined>>();
  private spare: Promise<Live | undefined> | undefined;
  private disposed = false;
  private readonly now: () => number;
  private readonly newId: () => string;

  constructor(private readonly options: SessionsOptions) {
    this.now = options.now ?? Date.now;
    this.newId = options.newId ?? (() => crypto.randomUUID());
  }

  /** What `seen` says this conversation's session has been shown. */
  seen(key: string) {
    return this.options.store.get(key)?.seen ?? 0;
  }
  /** Conversations with a running process, and whether each is working. */
  snapshot() {
    return [
      ...[...this.live].map(([key, live]) => ({
        key,
        busy: live.process.busy,
      })),
      ...[...this.once.keys()].map((key) => ({ key, busy: true })),
    ];
  }

  /** Starts a spare process unless one exists or the agent is at its limit. */
  warm() {
    if (this.disposed || this.spare || this.count() >= MAX_LIVE) return;
    const spare = this.start(undefined).catch((error) => {
      console.warn("Claude Code could not start a spare session", error);
      return undefined;
    });
    this.spare = spare;
    void spare.then((live) => {
      // A spare that dies, or was started with old settings, is not kept.
      live?.process.exited.then(() => {
        if (this.spare === spare) this.spare = undefined;
      });
    });
  }

  /** Sends `text` into the conversation `key`, and resolves when the session
   * has answered it. `steer` replaces `text` when the session is mid-turn;
   * `fresh` builds it again for a new session when the saved one is lost. */
  async deliver(
    key: string,
    text: string,
    seen: number,
    {
      steer = text,
      fresh,
    }: Readonly<{ steer?: string; fresh?: () => Promise<string> }> = {},
  ): Promise<Settled> {
    if (this.disposed) return { ok: false, error: "Claude Code was stopped" };
    // A message that arrives while its conversation's process starts uses it.
    // Checked without awaiting first, so the first message opens it at once.
    const opening = this.opening.get(key);
    if (opening) await opening;
    let live = this.live.get(key);
    if (
      live &&
      (!live.process.running ||
        (live.fingerprint !== this.options.fingerprint() && !live.process.busy))
    ) {
      this.drop(key, live);
      live = undefined;
    }
    if (live) {
      this.touch(key, live, seen);
      return this.answer(key, live, live.process.busy ? steer : text);
    }
    const saved = this.options.store.get(key);
    // A new conversation takes the spare's place; any other start needs room,
    // reserved here, before anything is awaited.
    if ((saved || !this.spare) && !this.room())
      return {
        ok: false,
        error: `Claude Code is already working in ${MAX_LIVE} conversations; try again when one finishes`,
      };
    const starting = (async () =>
      saved
        ? this.start(saved.id)
        : ((await this.claimSpare()) ?? this.start(undefined)))().catch(
      (error) => {
        console.warn("Claude Code could not start", error);
        return undefined;
      },
    );
    this.opening.set(key, starting);
    live = await starting;
    if (this.opening.get(key) === starting) this.opening.delete(key);
    if (live && !this.disposed) this.adopt(key, live, seen);
    if (!live) return { ok: false, error: "Claude Code could not start" };
    if (this.disposed) {
      void live.process.kill();
      return { ok: false, error: "Claude Code was stopped" };
    }
    const delivered = await this.answer(key, live, text);
    // A saved session that no longer loads starts over rather than failing
    // every message in the conversation, shown the conversation so far.
    if (!delivered.ok && saved && !live.process.started) {
      this.options.store.delete(key);
      this.drop(key, live);
      return this.deliver(key, fresh ? await fresh() : text, seen);
    }
    return delivered;
  }

  /** Runs `text` as one turn in a new session, then stops its process. The
   * session is not saved, so it never displaces a conversation's saved
   * session and is never resumed. `label` names it in the snapshot. When every
   * process is busy it waits for one, as Janet retries a busy wake; it gives
   * up, and stops the turn, after `deadline` ms of waiting or of running. */
  async runOnce(
    label: string,
    text: string,
    deadline = ONCE_DEADLINE_MS,
  ): Promise<Settled> {
    const until = Date.now() + deadline;
    // Take the spare's place, or reserve room, before anything is awaited.
    while (!this.disposed && !this.spare && !this.room()) {
      if (Date.now() >= until)
        return {
          ok: false,
          error: `Claude Code was running ${MAX_LIVE} sessions for ${Math.round(deadline / 1000)}s; this turn was given up`,
        };
      await new Promise((resolve) => setTimeout(resolve, ROOM_POLL_MS));
    }
    if (this.disposed) return { ok: false, error: "Claude Code was stopped" };
    const key = `${label} #${++this.onceCount}`;
    this.once.set(key, undefined);
    const spare = this.spare ? this.claimSpare() : undefined;
    // The spare is taken: start the next one now, not when this turn ends.
    this.warm();
    this.options.onChange?.();
    let live: Live | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      live = (await spare) ?? (await this.start(undefined));
      if (this.disposed) return { ok: false, error: "Claude Code was stopped" };
      // A key no conversation uses: its tools have no default destination.
      live.process.conversation = key;
      this.once.set(key, live);
      return await Promise.race([
        live.process.send(text),
        new Promise<Settled>((resolve) => {
          timer = setTimeout(
            () =>
              resolve({
                ok: false,
                error: `The turn ran past its ${Math.round(deadline / 1000)}s deadline and was stopped`,
              }),
            deadline,
          );
        }),
      ]);
    } catch (error) {
      console.warn("Claude Code could not start a one-off session", error);
      return { ok: false, error: "Claude Code could not start" };
    } finally {
      clearTimeout(timer);
      this.once.delete(key);
      void live?.process.kill();
      this.options.onChange?.();
      this.warm();
    }
  }

  /** Stops idle processes started with old settings and replaces the spare;
   * busy ones stop when they next fall idle. */
  reconfigure() {
    const fingerprint = this.options.fingerprint();
    for (const [key, live] of this.live)
      if (live.fingerprint !== fingerprint && !live.process.busy)
        this.drop(key, live);
    const spare = this.spare;
    this.spare = undefined;
    void spare?.then((live) => live?.process.kill());
    this.warm();
  }

  dispose() {
    this.disposed = true;
    for (const [key, live] of this.live) this.drop(key, live);
    for (const live of this.once.values()) void live?.process.kill();
    void this.spare?.then((live) => live?.process.kill());
    this.spare = undefined;
  }

  private async start(resume: string | undefined): Promise<Live> {
    // Read before `launch` reads the settings, so it never describes newer ones.
    const fingerprint = this.options.fingerprint();
    const launch = await this.options.launch();
    const process = await ClaudeProcess.start(this.options.spawn, {
      ...launch,
      ...(resume ? { resume } : { sessionId: this.newId() }),
    });
    if (fingerprint === this.options.fingerprint() || this.disposed)
      return { process, fingerprint, used: this.now() };
    // Settings saved while it started: it would work with the old ones.
    void process.kill();
    return this.start(resume);
  }
  private async claimSpare() {
    const spare = this.spare;
    this.spare = undefined;
    const live = await spare;
    if (
      live?.process.running &&
      live.fingerprint === this.options.fingerprint()
    )
      return live;
    void live?.process.kill();
    return undefined;
  }
  private adopt(key: string, live: Live, seen: number) {
    live.process.conversation = key;
    this.live.set(key, live);
    this.touch(key, live, seen);
    this.options.onChange?.();
    void live.process.exited.then(() => {
      if (this.live.get(key) === live) this.drop(key, live);
    });
    this.warm();
  }
  private async answer(key: string, live: Live, text: string) {
    const settled = await live.process.send(text);
    if (this.live.get(key) === live) {
      this.touch(key, live);
      if (live.fingerprint !== this.options.fingerprint() && !live.process.busy)
        this.drop(key, live);
    }
    return settled;
  }
  /** Saves the session and restarts its idle countdown. */
  private touch(key: string, live: Live, seen?: number) {
    live.used = this.now();
    clearTimeout(live.timer);
    live.timer = setTimeout(() => {
      if (live.process.busy) this.touch(key, live);
      else if (this.live.get(key) === live) this.drop(key, live);
    }, IDLE_MS);
    const id = live.process.sessionId;
    if (!id) return;
    const prior = this.options.store.get(key);
    this.options.store.set(key, {
      id,
      seen: Math.max(seen ?? 0, prior?.id === id ? prior.seen : 0),
      at: live.used,
    });
  }
  /** Processes running or starting, the spare included. */
  private count() {
    return (
      this.live.size + this.opening.size + this.once.size + (this.spare ? 1 : 0)
    );
  }
  /** Makes room for one more process by stopping the longest-idle
   * conversation, else the spare. False when every process is working. */
  private room() {
    while (this.count() >= MAX_LIVE) {
      const idle = [...this.live]
        .filter(([, live]) => !live.process.busy)
        .sort(([, a], [, b]) => a.used - b.used)[0];
      if (idle) this.drop(...idle);
      else if (this.spare) {
        void this.spare.then((live) => live?.process.kill());
        this.spare = undefined;
      } else return false;
    }
    return true;
  }
  private drop(key: string, live: Live) {
    clearTimeout(live.timer);
    if (this.live.get(key) === live) {
      this.live.delete(key);
      this.options.onChange?.();
    }
    void live.process.kill();
  }
}

/** Sessions saved in local storage, by agent and then conversation. */
export function localSessions(
  storage: Storage,
  agent: string,
  name = "buzz.claude-code.sessions.v1",
): SessionStore {
  const readAll = (): Record<string, Record<string, SavedSession>> => {
    try {
      const value = JSON.parse(storage.getItem(name) ?? "{}");
      return value && typeof value === "object" ? value : {};
    } catch {
      return {};
    }
  };
  const update = (change: (sessions: Record<string, SavedSession>) => void) => {
    const all = readAll();
    const sessions = { ...(all[agent] ?? {}) };
    change(sessions);
    const kept = Object.entries(sessions)
      .sort(([, a], [, b]) => b.at - a.at)
      .slice(0, SAVED_LIMIT);
    all[agent] = Object.fromEntries(kept);
    try {
      storage.setItem(name, JSON.stringify(all));
    } catch (error) {
      console.warn("Claude Code sessions were not saved", error);
    }
  };
  return {
    get: (key) => {
      const saved = readAll()[agent]?.[key];
      return saved && typeof saved.id === "string" ? saved : undefined;
    },
    set: (key, session) =>
      update((sessions) => {
        sessions[key] = session;
      }),
    delete: (key) =>
      update((sessions) => {
        delete sessions[key];
      }),
  };
}

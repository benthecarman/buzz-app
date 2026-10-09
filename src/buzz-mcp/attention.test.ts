import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  checkedWrite,
  interestIds,
  show,
  watchSummaries,
} from "../features/agents2/attention-objects";
import type { AgentAttention } from "../features/agents2/service";
import type { AgentRecord } from "../features/agents2/store";
import { ATTENTION_TOOLS } from "./attention";
import { respond } from "./rpc";
import { TOOLS } from "./tools";

const channel = "0b8e2a3c-1d4f-4a5b-8c6d-7e8f9a0b1c2d";
const NOW = 10_000;

/** An agent's attention over one in-memory record, as the service builds it. */
function agent(options: { classifier?: boolean; enabled?: boolean } = {}) {
  let record: AgentRecord = {
    pubkey: "a".repeat(64),
    attention: {},
    config: {},
  };
  const now = () => Math.floor(Date.now() / 1000);
  const api: AgentAttention = {
    enabled: () => options.enabled ?? true,
    classifier: () => (options.classifier ? "available" : "unavailable"),
    show: (slug, interest) => show(record, slug, now(), interest),
    interests: (search) => interestIds(record, search),
    watches: (filter = {}) => watchSummaries(record, filter, now()),
    write: async (slug, value, write) => {
      record = await checkedWrite(record, slug, value, write, now());
      return show(record, slug, now());
    },
  };
  let id = 0;
  /** Calls tool `name` through the RPC, as Claude does. */
  const call = async (name: string, args: Record<string, unknown> = {}) => {
    const answer = (await respond(
      undefined,
      {},
      { id: ++id, method: "tools/call", params: { name, arguments: args } },
      { offered: true, api },
    )) as { result: { content: { text: string }[]; isError?: true } };
    const text = answer.result.content[0]?.text ?? "";
    if (answer.result.isError) throw new Error(text);
    return JSON.parse(text);
  };
  /** Counts a timer run, as the service does when it delivers one. */
  const ran = (slug: string, at: number, interval: number) => {
    const state = record.timers?.[slug];
    if (!state) throw new Error(`No timer state for ${slug}`);
    record = {
      ...record,
      timers: {
        ...record.timers,
        [slug]: { ...state, used: state.used + 1, nextDue: at + interval },
      },
    };
  };
  return { api, call, ran };
}

const ABSENT = "object-v1:absent";
const asks = {
  interest: "triage",
  id: "asks",
  type: "event",
  since: 1,
  channels: [channel],
  kinds: [9],
};

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(NOW * 1000);
});
afterEach(() => vi.useRealTimers());

describe("the tool list", () => {
  const list = (attention?: Parameters<typeof respond>[3]) =>
    respond(undefined, {}, { id: 1, method: "tools/list" }, attention).then(
      (answer) =>
        (
          (answer as { result: { tools: { name: string }[] } }).result.tools ??
          []
        ).map((tool) => tool.name),
    );

  it("offers the attention tools only when the session is given them", async () => {
    expect(await list()).toEqual(TOOLS.map((tool) => tool.name));
    expect(await list({ offered: false })).toEqual(
      TOOLS.map((tool) => tool.name),
    );
    expect(await list({ offered: true })).toEqual(
      [...TOOLS, ...ATTENTION_TOOLS].map((tool) => tool.name),
    );
  });

  it("has Janet's verbs, and always has the classifier fields", () => {
    expect(ATTENTION_TOOLS.map((tool) => tool.name)).toEqual([
      "interest_set",
      "interest_show",
      "interest_list",
      "interest_remove",
      "watch_add",
      "watch_update",
      "watch_enable",
      "watch_disable",
      "watch_remove",
      "watch_rearm",
      "watch_show",
      "watch_list",
    ]);
    for (const name of ["watch_add", "watch_update"])
      expect(
        ATTENTION_TOOLS.find((tool) => tool.name === name)?.inputSchema
          .properties,
      ).toHaveProperty("classifier");
  });

  it("refuses an attention call that the session was not given", async () => {
    const { api } = agent();
    // A ready client, so the refusal comes from the tool set, not readiness.
    const answer = (await respond(
      {} as never,
      {},
      { id: 1, method: "tools/call", params: { name: "interest_list" } },
      { offered: false, api },
    )) as { result: { isError?: true; content: { text: string }[] } };
    expect(answer.result.isError).toBe(true);
    expect(answer.result.content[0]?.text).toMatch(/No tool interest_list/);
  });
});

describe("the attention tools", () => {
  it("creates, shows and replaces an Interest with expected-state tokens", async () => {
    const { call } = agent();
    const created = await call("interest_set", {
      id: "triage",
      instructions: "Triage asks.",
      expected_state: ABSENT,
    });
    expect(created).toMatchObject({
      classifier: "unavailable",
      result: "found",
      object: { id: "triage", instructions: "Triage asks." },
    });
    await expect(
      call("interest_set", {
        id: "triage",
        instructions: "Stale.",
        expected_state: ABSENT,
      }),
    ).rejects.toThrow(/^conflict:[\s\S]*\(classifier: unavailable\)$/);
    const shown = await call("interest_show", { id: "triage" });
    expect(shown.expected_state).toBe(created.expected_state);
    await call("interest_set", {
      id: "triage",
      instructions: "Triage every ask.",
      expected_state: shown.expected_state,
    });
    expect(await call("interest_list")).toEqual({
      classifier: "unavailable",
      interests: ["triage"],
    });
  });

  it("keeps an Interest until its watches are gone", async () => {
    const { call } = agent();
    await call("interest_set", {
      id: "triage",
      instructions: "T.",
      expected_state: ABSENT,
    });
    await call("watch_add", { ...asks, expected_state: ABSENT });
    const interest = await call("interest_show", { id: "triage" });
    await expect(
      call("interest_remove", {
        id: "triage",
        expected_state: interest.expected_state,
      }),
    ).rejects.toThrow(/watches/);
    const watch = await call("watch_show", { id: "asks" });
    await call("watch_remove", {
      id: "asks",
      expected_state: watch.expected_state,
    });
    await call("interest_remove", {
      id: "triage",
      expected_state: interest.expected_state,
    });
    expect((await call("interest_list")).interests).toEqual([]);
  });

  it("scopes watch writes to the Interest the agent works for", async () => {
    const { call } = agent();
    for (const id of ["triage", "other"])
      await call("interest_set", {
        id,
        instructions: id,
        expected_state: ABSENT,
      });
    await call("watch_add", { ...asks, expected_state: ABSENT });
    const watch = await call("watch_show", { id: "asks" });
    await expect(
      call("watch_disable", {
        id: "asks",
        interest: "other",
        expected_state: watch.expected_state,
      }),
    ).rejects.toThrow(/^wrong-scope:/);
  });

  it("changes only the fields given, and clears optional ones", async () => {
    const { call } = agent();
    await call("interest_set", {
      id: "triage",
      instructions: "T.",
      expected_state: ABSENT,
    });
    const added = await call("watch_add", {
      ...asks,
      name: "Asks",
      filter: "!is_reply",
      expected_state: ABSENT,
    });
    const updated = await call("watch_update", {
      id: "asks",
      kinds: [9, 40002],
      clear: ["filter"],
      expected_state: added.expected_state,
    });
    expect(updated.object).toEqual({
      id: "asks",
      type: "event",
      interest_id: "triage",
      name: "Asks",
      enabled: true,
      status: "enabled",
      since: 1,
      channels: [channel],
      kinds: [9, 40002],
    });
    await expect(
      call("watch_update", {
        id: "asks",
        prompt: "no",
        expected_state: updated.expected_state,
      }),
    ).rejects.toThrow(/^invalid: Not a field of this watch type: prompt/);
  });

  it("requires since for an event watch", async () => {
    const { call } = agent();
    await call("interest_set", {
      id: "triage",
      instructions: "T.",
      expected_state: ABSENT,
    });
    const { since: _, ...noSince } = asks;
    await expect(
      call("watch_add", { ...noSince, expected_state: ABSENT }),
    ).rejects.toThrow(/since/);
    const now = await call("watch_add", {
      ...asks,
      since: "now",
      expected_state: ABSENT,
    });
    expect(now.object.since).toBe(NOW);
  });

  it("runs a timer once by default, lists it as spent, and rearms only with room", async () => {
    const { call, ran } = agent();
    await call("interest_set", {
      id: "triage",
      instructions: "T.",
      expected_state: ABSENT,
    });
    const added = await call("watch_add", {
      interest: "triage",
      id: "nudge",
      type: "timer",
      prompt: "Check the queue.",
      interval_secs: 60,
      expected_state: ABSENT,
    });
    expect(added.object).toMatchObject({
      max_occurrences: 1,
      armed_at: NOW,
      status: "armed",
      used: 0,
      next_due: NOW + 60,
    });
    vi.setSystemTime((NOW + 61) * 1000);
    ran("watch/nudge", NOW + 61, 60);
    expect((await call("watch_list", { status: "spent" })).watches).toEqual([
      { id: "nudge", interest_id: "triage", type: "timer", status: "spent" },
    ]);
    const spent = await call("watch_show", { id: "nudge" });
    await expect(
      call("watch_rearm", {
        id: "nudge",
        expected_state: spent.expected_state,
      }),
    ).rejects.toThrow(/spent/);
    const rearmed = await call("watch_rearm", {
      id: "nudge",
      max_occurrences: 3,
      expected_state: spent.expected_state,
    });
    expect(rearmed.object).toMatchObject({
      armed_at: NOW + 61,
      max_occurrences: 3,
      status: "armed",
      used: 1,
      next_due: NOW + 121,
    });
  });

  it("filters the watch list by type, status and search", async () => {
    const { call } = agent();
    await call("interest_set", {
      id: "triage",
      instructions: "T.",
      expected_state: ABSENT,
    });
    await call("watch_add", { ...asks, name: "Asks", expected_state: ABSENT });
    await call("watch_add", {
      ...asks,
      id: "quiet",
      enabled: false,
      expected_state: ABSENT,
    });
    const ids = async (args: Record<string, unknown>) =>
      (await call("watch_list", args)).watches.map((w: { id: string }) => w.id);
    expect(await ids({})).toEqual(["asks", "quiet"]);
    expect(await ids({ status: "disabled" })).toEqual(["quiet"]);
    expect(await ids({ type: "timer" })).toEqual([]);
    expect(await ids({ search: "Ask" })).toEqual(["asks"]);
  });

  it("refuses every call once the owner turns attention off", async () => {
    const { call } = agent({ enabled: false });
    await expect(call("interest_list")).rejects.toThrow(
      /Attention is off[\s\S]*\(classifier: unavailable\)/,
    );
  });

  it("never replaces a watch on add", async () => {
    const { call } = agent();
    await call("interest_set", {
      id: "triage",
      instructions: "T.",
      expected_state: ABSENT,
    });
    const added = await call("watch_add", {
      ...asks,
      name: "Asks",
      filter: "!is_reply",
      expected_state: ABSENT,
    });
    for (const expected_state of [added.expected_state, ABSENT])
      await expect(
        call("watch_add", { ...asks, expected_state }),
      ).rejects.toThrow(
        /^invalid-operation: watch asks exists; use watch_update/,
      );
    expect((await call("watch_show", { id: "asks" })).object).toMatchObject({
      name: "Asks",
      filter: "!is_reply",
    });
  });

  it("turns an event watch off and on, and refuses missing or stale watches", async () => {
    const { call } = agent();
    await call("interest_set", {
      id: "triage",
      instructions: "T.",
      expected_state: ABSENT,
    });
    const added = await call("watch_add", { ...asks, expected_state: ABSENT });
    const off = await call("watch_disable", {
      id: "asks",
      expected_state: added.expected_state,
    });
    expect(off.object).toMatchObject({ enabled: false, status: "disabled" });
    await expect(
      call("watch_enable", {
        id: "asks",
        expected_state: added.expected_state,
      }),
    ).rejects.toThrow(/^conflict:/);
    const on = await call("watch_enable", {
      id: "asks",
      expected_state: off.expected_state,
    });
    expect(on.object).toMatchObject({ enabled: true, status: "enabled" });
    for (const name of ["watch_enable", "watch_update", "watch_rearm"])
      await expect(
        call(name, { id: "none", expected_state: ABSENT }),
      ).rejects.toThrow(/^not-found:/);
    await expect(call("watch_update", { id: "asks" })).rejects.toThrow(
      /^invalid-operation: every write needs the expected_state/,
    );
  });

  it("turns a timer on only by rearming it, never from the past", async () => {
    const { call } = agent();
    await call("interest_set", {
      id: "triage",
      instructions: "T.",
      expected_state: ABSENT,
    });
    const added = await call("watch_add", {
      interest: "triage",
      id: "nudge",
      type: "timer",
      prompt: "Check.",
      interval_secs: 3600,
      max_occurrences: 5,
      expected_state: ABSENT,
    });
    const off = await call("watch_disable", {
      id: "nudge",
      expected_state: added.expected_state,
    });
    expect(off.object).toMatchObject({ status: "inactive", used: 0 });
    await expect(
      call("watch_enable", { id: "nudge", expected_state: off.expected_state }),
    ).rejects.toThrow(/^invalid-operation: use watch_rearm/);
    await expect(
      call("watch_rearm", {
        id: "nudge",
        at: NOW - 1,
        expected_state: off.expected_state,
      }),
    ).rejects.toThrow(/^invalid: at must be/);
    // Ten hours later, nothing was spent while it was off.
    vi.setSystemTime((NOW + 36_000) * 1000);
    const rearmed = await call("watch_rearm", {
      id: "nudge",
      expected_state: off.expected_state,
    });
    expect(rearmed.object).toMatchObject({
      enabled: true,
      status: "armed",
      used: 0,
      next_due: NOW + 36_000 + 3600,
    });
  });
});

describe("a classifier", () => {
  const classifier = {
    questions: { ask: { question: "Is this a request for help?" } },
  };
  const setup = async (available: boolean) => {
    const { call } = agent({ classifier: available });
    await call("interest_set", {
      id: "triage",
      instructions: "T.",
      expected_state: ABSENT,
    });
    return call;
  };

  it("is saved without a key, and every answer says it will not run", async () => {
    const call = await setup(false);
    const added = await call("watch_add", {
      ...asks,
      classifier,
      expected_state: ABSENT,
    });
    expect(added).toMatchObject({
      classifier: "unavailable",
      classifier_note: expect.stringMatching(/once a classifier is available/),
      object: { classifier },
    });
    expect((await call("watch_show", { id: "asks" })).classifier_note).toMatch(
      /pass unchecked/,
    );
    expect((await call("watch_list")).watches).toEqual([
      {
        id: "asks",
        interest_id: "triage",
        type: "event",
        status: "enabled",
        classified: true,
        classifier: "unavailable",
      },
    ]);
    const cleared = await call("watch_update", {
      id: "asks",
      clear: ["classifier"],
      expected_state: added.expected_state,
    });
    expect(cleared).not.toHaveProperty("classifier_note");
    expect(cleared.object).not.toHaveProperty("classifier");
  });

  it("carries no warning when a classifier can run", async () => {
    const call = await setup(true);
    const added = await call("watch_add", {
      ...asks,
      classifier,
      expected_state: ABSENT,
    });
    expect(added.classifier).toBe("available");
    expect(added).not.toHaveProperty("classifier_note");
    expect((await call("watch_list")).watches[0]).not.toHaveProperty(
      "classifier",
    );
  });
});

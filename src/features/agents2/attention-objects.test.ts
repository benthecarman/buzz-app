import { describe, expect, it } from "vitest";
import type { EventWatch, TimerWatch } from "./attention";
import {
  ABSENT,
  checkedWrite,
  interestIds,
  show,
  token,
  watchSummaries,
} from "./attention-objects";
import { setAttention, type AgentRecord } from "./store";

const channel = "0b8e2a3c-1d4f-4a5b-8c6d-7e8f9a0b1c2d";
const now = 10_000;
const interest = (instructions: string) =>
  ({ type: "interest", instructions }) as const;
const watch = (patch: Partial<EventWatch> = {}): EventWatch => ({
  type: "event",
  interest_id: "triage",
  enabled: true,
  since: 1,
  channels: [channel],
  kinds: [9],
  ...patch,
});
const timer = (patch: Partial<TimerWatch> = {}): TimerWatch => ({
  type: "timer",
  interest_id: "triage",
  prompt: "check",
  enabled: true,
  interval_secs: 60,
  armed_at: now,
  max_occurrences: 1,
  expires_at: null,
  ...patch,
});
function record(): AgentRecord {
  let value: AgentRecord = {
    pubkey: "a".repeat(64),
    attention: {},
    config: {},
  };
  value = setAttention(value, "interest/triage", interest("Triage."), now);
  value = setAttention(value, "interest/other", interest("Other."), now);
  value = setAttention(value, "watch/asks", watch({ name: "Asks" }), now);
  return value;
}
const write = async (
  value: AgentRecord,
  slug: string,
  next: Parameters<typeof checkedWrite>[2],
  scope?: string,
) =>
  checkedWrite(
    value,
    slug,
    next,
    {
      expected: (await show(value, slug, now)).expected_state,
      ...(scope ? { interest: scope } : {}),
    },
    now,
  );

describe("expected-state tokens", () => {
  it("are absent for a missing object, and follow content, not key order", async () => {
    expect(await show(record(), "watch/none", now)).toEqual({
      result: "not-found",
      object: null,
      expected_state: ABSENT,
    });
    const a = await token({ id: "x", type: "interest", instructions: "i" });
    const b = await token({ instructions: "i", type: "interest", id: "x" });
    expect(a).toMatch(/^object-v1:[0-9a-f]{64}$/);
    expect(a).toBe(b);
    expect(
      await token({ id: "x", type: "interest", instructions: "j" }),
    ).not.toBe(a);
  });

  it("refuse a write without a token, with a stale one, or creating over an object", async () => {
    const start = record();
    const shown = await show(start, "watch/asks", now);
    await expect(
      checkedWrite(start, "watch/asks", null, { expected: "" }, now),
    ).rejects.toThrow(/^invalid-operation:/);
    const edited = await write(start, "watch/asks", watch({ name: "Edited" }));
    await expect(
      checkedWrite(
        edited,
        "watch/asks",
        null,
        { expected: shown.expected_state },
        now,
      ),
    ).rejects.toThrow(/^conflict:/);
    await expect(
      checkedWrite(edited, "watch/asks", watch(), { expected: ABSENT }, now),
    ).rejects.toThrow(/^conflict:/);
    // An unrelated object's edit does not conflict.
    const other = await write(edited, "interest/other", interest("Changed."));
    expect((await show(other, "watch/asks", now)).expected_state).toBe(
      (await show(edited, "watch/asks", now)).expected_state,
    );
  });
});

describe("checked writes", () => {
  it("keep a scoped caller to its own Interest", async () => {
    const start = record();
    await expect(show(start, "watch/asks", now, "other")).rejects.toThrow(
      /^wrong-scope:/,
    );
    await expect(
      write(start, "watch/new", watch({ interest_id: "other" }), "triage"),
    ).rejects.toThrow(/^wrong-scope:/);
    await expect(
      write(start, "interest/other", interest("x"), "triage"),
    ).rejects.toThrow(/^wrong-scope:/);
    const added = await write(start, "watch/new", watch(), "triage");
    expect(added.attention["watch/new"]).toBeDefined();
  });

  it("need the watch's Interest, and never move or retype a watch", async () => {
    const start = record();
    await expect(
      write(start, "watch/lost", watch({ interest_id: "missing" })),
    ).rejects.toThrow(/^not-found:.*set it first/);
    await expect(
      write(start, "watch/asks", watch({ interest_id: "other" })),
    ).rejects.toThrow(/^invalid-operation:.*move/);
    await expect(write(start, "watch/asks", timer())).rejects.toThrow(
      /^invalid-operation:.*type/,
    );
    await expect(write(start, "watch/asks", interest("x"))).rejects.toThrow(
      /^invalid:/,
    );
  });

  it("remove an Interest only after its watches", async () => {
    const start = record();
    await expect(write(start, "interest/triage", null)).rejects.toThrow(
      /^invalid-operation:.*asks/,
    );
    const bare = await write(start, "watch/asks", null);
    const gone = await write(bare, "interest/triage", null);
    expect(interestIds(gone)).toEqual(["other"]);
  });

  it("report the format's own problems as invalid", async () => {
    await expect(
      write(record(), "watch/asks", watch({ since: 0 })),
    ).rejects.toThrow(/^invalid: since/);
    // Before any scope or Interest check could name a field it lacks.
    await expect(
      write(record(), "watch/junk", "junk" as never, "triage"),
    ).rejects.toThrow(/^invalid:/);
  });

  it("keep an agent's timers at least a minute apart, except one the owner set", async () => {
    await expect(
      write(record(), "watch/fast", timer({ interval_secs: 1 })),
    ).rejects.toThrow(/^invalid: interval_secs must be at least 60/);
    const owned = setAttention(
      record(),
      "watch/fast",
      timer({ interval_secs: 1 }),
      now,
    );
    const edited = await write(
      owned,
      "watch/fast",
      timer({ interval_secs: 1, prompt: "faster" }),
    );
    expect(edited.attention["watch/fast"]?.value).toMatchObject({
      prompt: "faster",
    });
  });

  it("cap the attention one agent can store", async () => {
    let value = record();
    const long = "x".repeat(16_000);
    for (let index = 0; index < 16; index++)
      value = await write(value, `interest/i${index}`, interest(long));
    await expect(write(value, "interest/more", interest(long))).rejects.toThrow(
      /^invalid: Your attention would take more than 256 KB/,
    );
    // Removing is always allowed.
    value = await write(value, "interest/i0", null);
    expect(value.attention).not.toHaveProperty("interest/i0");
  });

  it("refuse to replace an object this app could not read", async () => {
    const value: AgentRecord = {
      ...record(),
      skipped: {
        "watch/new": {
          slug: "watch/new",
          value: { type: "future" },
          modifiedAt: 1,
          problem: "Unknown type",
        },
      },
    };
    for (const next of [watch(), null])
      await expect(write(value, "watch/new", next)).rejects.toThrow(
        /^invalid-operation: watch\/new is stored in a form this app cannot read/,
      );
  });
});

describe("summaries", () => {
  it("give each watch's status and filter by type, status and search", async () => {
    let value = record();
    value = await write(value, "watch/once", timer());
    value = await write(value, "watch/off", timer({ enabled: false }));
    value = await write(
      value,
      "watch/done",
      timer({ armed_at: now - 600, max_occurrences: 2 }),
    );
    value = await write(
      value,
      "watch/judged",
      watch({
        enabled: false,
        classifier: { questions: { a: { question: "q" } } },
      }),
    );
    expect(watchSummaries(value, {}, now)).toEqual([
      {
        id: "asks",
        interest_id: "triage",
        name: "Asks",
        type: "event",
        status: "enabled",
      },
      { id: "done", interest_id: "triage", type: "timer", status: "spent" },
      {
        id: "judged",
        interest_id: "triage",
        type: "event",
        status: "disabled",
        classified: true,
      },
      { id: "off", interest_id: "triage", type: "timer", status: "inactive" },
      { id: "once", interest_id: "triage", type: "timer", status: "armed" },
    ]);
    const ids = (filter: Parameters<typeof watchSummaries>[1]) =>
      watchSummaries(value, filter, now).map((summary) => summary.id);
    expect(ids({ type: "timer", status: "spent" })).toEqual(["done"]);
    expect(ids({ search: "As" })).toEqual(["asks"]);
    expect(ids({ search: "as" })).toEqual(["asks"]);
    expect(ids({ interest: "other" })).toEqual([]);
    expect((await show(value, "watch/once", now)).object).toMatchObject({
      status: "armed",
      used: 0,
      next_due: now + 60,
    });
  });
});

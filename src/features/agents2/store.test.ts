import { expect, it } from "vitest";
import {
  STORAGE_KEY,
  readRecords,
  setAttention,
  writeRecords,
  type AgentRecord,
} from "./store";
import { memoryStorage } from "./test-fakes";

const pubkey = "a".repeat(64);
const record: AgentRecord = {
  pubkey,
  attention: {},
  config: { word: "hi" },
};

it("round-trips records and drops attention objects a reader would ignore", () => {
  const storage = memoryStorage();
  const saved = setAttention(
    record,
    "interest/default",
    { type: "interest", instructions: "Be brief." },
    7,
  );
  writeRecords(storage, { [pubkey]: saved });
  expect(readRecords(storage)).toEqual({ [pubkey]: saved });
  const stored = JSON.parse(storage.getItem(STORAGE_KEY) as string);
  stored.agents[pubkey].attention["watch/bad"] = {
    slug: "watch/bad",
    value: { type: "event" },
    modifiedAt: 1,
  };
  stored.agents.nonsense = record;
  storage.setItem(STORAGE_KEY, JSON.stringify(stored));
  const read = readRecords(storage);
  expect(read[pubkey]?.attention).toEqual(saved.attention);
  expect(read).not.toHaveProperty("nonsense");
  // The invalid object is reported, and written back as it was.
  expect(read[pubkey]?.skipped?.["watch/bad"]).toMatchObject({
    value: { type: "event" },
    problem: expect.stringMatching(/Missing/),
  });
  writeRecords(storage, read);
  expect(
    JSON.parse(storage.getItem(STORAGE_KEY) as string).agents[pubkey].attention[
      "watch/bad"
    ],
  ).toEqual({ slug: "watch/bad", value: { type: "event" }, modifiedAt: 1 });
  // Fixing it moves it back to the attention it applies.
  const fixed = setAttention(read[pubkey] as AgentRecord, "watch/bad", null);
  expect(fixed.skipped).toEqual({});
});

it("skips objects over the count limit in slug order, and refuses to add more", () => {
  const storage = memoryStorage();
  const attention = Object.fromEntries(
    Array.from({ length: 101 }, (_, n) => {
      const slug = `interest/i${String(n).padStart(3, "0")}`;
      return [
        slug,
        { slug, value: { type: "interest", instructions: "x" }, modifiedAt: 1 },
      ];
    }),
  );
  storage.setItem(
    STORAGE_KEY,
    JSON.stringify({
      version: 1,
      agents: { [pubkey]: { ...record, attention } },
    }),
  );
  const read = readRecords(storage)[pubkey] as AgentRecord;
  expect(Object.keys(read.attention)).toHaveLength(100);
  expect(Object.keys(read.skipped ?? {})).toEqual(["interest/i100"]);
  const { "interest/i099": _, ...full } = read.attention;
  expect(() =>
    setAttention({ ...read, attention: full }, "interest/new", {
      type: "interest",
      instructions: "x",
    }),
  ).not.toThrow();
  expect(() =>
    setAttention(read, "interest/new", { type: "interest", instructions: "x" }),
  ).toThrow(/at most 100/);
});

it("keeps a timer's budget across edits; only a rearm moves its deadline", () => {
  const timer = {
    type: "timer",
    interest_id: "x",
    prompt: "tick",
    enabled: true,
    interval_secs: 3600,
    armed_at: 100,
    max_occurrences: 3,
    expires_at: null,
  } as const;
  const armed = setAttention(record, "watch/t", timer, 100);
  expect(armed.timers?.["watch/t"]).toEqual({
    armedAt: 100,
    nextDue: 3700,
    used: 0,
  });
  const ran = {
    ...armed,
    timers: { "watch/t": { armedAt: 100, nextDue: 7300, used: 1 } },
  };
  const later = 100 + 10 * 3600;
  // A new prompt or interval, pausing and resuming: none of them spends an
  // occurrence or moves the deadline. A new interval applies from the next run.
  for (const edit of [
    { prompt: "tock" },
    { interval_secs: 60 },
    { enabled: false },
    { enabled: true },
  ])
    expect(
      setAttention(ran, "watch/t", { ...timer, ...edit }, later).timers,
    ).toEqual(ran.timers);
  // A rearm is a new deadline that keeps what was used, as in Janet, even
  // when it is armed in the past.
  expect(
    setAttention(ran, "watch/t", { ...timer, armed_at: later }, later).timers?.[
      "watch/t"
    ],
  ).toEqual({ armedAt: later, nextDue: later + 3600, used: 1 });
  expect(
    setAttention(ran, "watch/t", { ...timer, armed_at: 200 }, later).timers?.[
      "watch/t"
    ],
  ).toEqual({ armedAt: 200, nextDue: 3800, used: 1 });
  // Without saved state, what was already due counts as used.
  expect(
    setAttention(record, "watch/t", timer, 100 + 2 * 3600 + 5).timers?.[
      "watch/t"
    ],
  ).toEqual({ armedAt: 100, nextDue: 100 + 3 * 3600, used: 2 });
});

it("validates, replaces and deletes one object at a time", () => {
  expect(() =>
    setAttention(record, "interest/x", { type: "interest", instructions: " " }),
  ).toThrow(/empty/);
  const one = setAttention(
    record,
    "interest/x",
    { type: "interest", instructions: "a" },
    1,
  );
  const two = setAttention(
    one,
    "interest/x",
    { type: "interest", instructions: "b" },
    2,
  );
  expect(two.attention["interest/x"]).toEqual({
    slug: "interest/x",
    value: { type: "interest", instructions: "b" },
    modifiedAt: 2,
  });
  expect(setAttention(two, "interest/x", null).attention).toEqual({});
});

it("reads nothing from missing or malformed storage", () => {
  const storage = memoryStorage();
  expect(readRecords(storage)).toEqual({});
  storage.setItem(STORAGE_KEY, "{");
  expect(readRecords(storage)).toEqual({});
});

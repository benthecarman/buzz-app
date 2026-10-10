import { afterEach, expect, test, vi } from "vitest";
import {
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import {
  finalizeEvent,
  generateSecretKey,
  getPublicKey,
  nip44,
} from "nostr-tools";
import { openArchive } from "./archive.mjs";
const owner = generateSecretKey(),
  agent = generateSecretKey();
const viewer = getPublicKey(owner),
  sender = getPublicKey(agent),
  conversationKey = nip44.v2.utils.getConversationKey(agent, viewer),
  community = "https://a.test";
const dirs = [];
afterEach(() => {
  vi.restoreAllMocks();
  for (const dir of dirs.splice(0))
    rmSync(dir, { recursive: true, force: true });
});
function path() {
  const dir = mkdtempSync(join(tmpdir(), "buzz-archive-"));
  dirs.push(dir);
  return join(dir, "events.sqlite3");
}
function event(serial = 0, kind = 24200, patch = {}, key = agent) {
  const plaintext = JSON.stringify({
    kind: "turn_started",
    channelId: "private-channel",
    payload: "archive-secret-marker",
    serial,
  });
  return finalizeEvent(
    {
      kind,
      created_at: Math.floor(Date.now() / 1000),
      tags: [
        ["p", viewer],
        ["agent", getPublicKey(key)],
        ["frame", "telemetry"],
      ],
      content: nip44.v2.encrypt(
        plaintext,
        key === agent
          ? conversationKey
          : nip44.v2.utils.getConversationKey(key, viewer),
      ),
      ...patch,
    },
    key,
  );
}
function harness(file = ":memory:") {
  const store = openArchive(file);
  const request = (input, who = viewer, where = community) =>
    store.request(who, where, input, owner);
  const settings = () => request({ action: "settings" });
  settings();
  return {
    store,
    request,
    settings,
    read: (patch = {}) => request({ action: "read", kind: 24200, ...patch }),
    ingest: (row, revision = settings().revision) =>
      store.ingest(viewer, community, row, revision, owner),
  };
}
test("real SQLite survives reopen with only ciphertext, dedup, and account/community isolation", () => {
  const file = path();
  let h = harness(file);
  const row = event();
  h.ingest(row);
  h.ingest(row);
  h.ingest(event(1, 44200));
  expect(h.read().records).toHaveLength(1);
  expect(
    h.request({ action: "read", kind: 24200 }, viewer, "https://b.test")
      .records,
  ).toEqual([]);
  expect(
    h.request({ action: "read", kind: 24200 }, "b".repeat(64)).records,
  ).toEqual([]);
  for (const name of readdirSync(dirs.at(-1)))
    expect(
      readFileSync(join(dirs.at(-1), name)).includes(
        Buffer.from("archive-secret-marker"),
      ),
    ).toBe(false);
  h.store.close();
  h = harness(file);
  expect(h.read().records[0]).toMatchObject({ id: row.id, agent: sender });
  expect(h.read({ kind: 44200 }).records).toHaveLength(1);
  h.request({ action: "clear", kind: 24200 });
  expect(h.read().records).toEqual([]);
  expect(h.read({ kind: 44200 }).records).toHaveLength(1);
  h.store.close();
});
test("keyset paging walks hidden/bad rows, scopes agent reads, and one bad row cannot poison a page", () => {
  const file = path(),
    h = harness(file),
    other = generateSecretKey();
  const db = new DatabaseSync(file);
  try {
    // Paging exercises stored envelopes, not repeated fresh-ingest validation.
    // Seed real signed/encrypted rows in one transaction; keep decoding on reads.
    const insert = db.prepare(
      "INSERT INTO archive_events(viewer,community,subscription,id,agent,kind,created,received,bytes,envelope) VALUES (?,?,'observer',?,?,24200,?,?,?,?)",
    );
    db.exec("BEGIN");
    for (let i = 0; i < 205; i++) {
      const row = event(i),
        raw = JSON.stringify(row);
      insert.run(
        viewer,
        community,
        row.id,
        row.pubkey,
        row.created_at,
        row.created_at,
        Buffer.byteLength(raw),
        raw,
      );
    }
    db.exec("COMMIT");
    const quiet = event(999, 24200, {}, other);
    h.ingest(quiet);
    db.prepare("UPDATE archive_events SET envelope='broken' WHERE id=?").run(
      quiet.id,
    );
    const first = h.read();
    expect(first.records).toHaveLength(99);
    expect(first.skipped).toBe(1);
    const second = h.read({ before: first.before });
    expect(second.records).toHaveLength(100);
    const third = h.read({ before: second.before });
    expect(third.records).toHaveLength(6);
    expect(third.before).toBeNull();
    const ids = [...first.records, ...second.records, ...third.records].map(
      (row) => row.id,
    );
    expect(new Set(ids).size).toBe(205);
    expect(h.read({ agent: getPublicKey(other) })).toMatchObject({
      records: [],
      skipped: 1,
    });
    expect(() => h.read({ before: 0 })).toThrow();
    expect(() => h.read({ agent: "bad" })).toThrow();
  } finally {
    // Closing also rolls back a seed that failed before COMMIT.
    db.close();
    h.store.close();
  }
});
test("fresh ingest gate rejects signature, recipient, duplicate tags, stale, unsupported kind and oversized content", () => {
  const h = harness();
  const valid = event();
  for (const invalid of [
    { ...valid, sig: "0".repeat(128) },
    event(1, 9),
    event(2, 24200, { created_at: Math.floor(Date.now() / 1000) - 301 }),
    event(3, 24200, { tags: [...valid.tags, ["p", viewer]] }),
    event(4, 44200, {
      tags: [
        ["p", "b".repeat(64)],
        ["agent", sender],
      ],
    }),
    event(5, 44200, { content: "x".repeat(87473) }),
  ])
    expect(() => h.ingest(invalid)).toThrow();
  expect(h.read().records).toEqual([]);
  h.store.close();
});
test("separate policies, revision fences, expiry, agent fairness and size accounting are enforced by SQL", () => {
  const file = path(),
    h = harness(file),
    other = generateSecretKey();
  const quiet = event(0, 24200, {}, other);
  h.ingest(quiet);
  h.ingest(event(1, 44200));
  const db = new DatabaseSync(file);
  db.exec("UPDATE archive_subscriptions SET budget=8000 WHERE name='observer'");
  for (let i = 1; i < 15; i++) h.ingest(event(i));
  expect(h.read().records.some((row) => row.id === quiet.id)).toBe(true);
  expect(h.settings().bytes).toBeLessThan(9000);
  const before = h.read().records.length;
  h.request({
    action: "configure",
    ...h.settings(),
    observer: false,
    metrics: true,
    observerDays: 1,
  });
  expect(() => h.ingest(event(90), 0)).toThrow("revision");
  h.ingest(event(91));
  expect(h.read().records).toHaveLength(before);
  h.ingest(event(92, 44200));
  expect(h.read({ kind: 44200 }).records).toHaveLength(2);
  const now = Date.now();
  vi.spyOn(Date, "now").mockReturnValue(now + 86401 * 1000);
  expect(h.read().records).toEqual([]);
  expect(h.read({ kind: 44200 }).records).toHaveLength(2);
  expect(db.prepare("PRAGMA auto_vacuum").get().auto_vacuum).toBe(2);
  db.close();
  h.store.close();
});
test("newer-version database stays byte-identical and malformed databases are not replaced", () => {
  const file = path();
  const db = new DatabaseSync(file);
  db.exec(
    "PRAGMA user_version=3;CREATE TABLE future(data TEXT);INSERT INTO future VALUES('keep')",
  );
  db.close();
  const before = readFileSync(file);
  expect(() => openArchive(file)).toThrow("newer");
  expect(readFileSync(file)).toEqual(before);
});

test("community pool evicts the oldest agent while each remains below its own cap", () => {
  const file = path(),
    h = harness(file);
  const rows = Array.from({ length: 5 }, (_, i) =>
    event(i, 24200, {}, generateSecretKey()),
  );
  const size = Buffer.byteLength(JSON.stringify(rows[0]));
  const db = new DatabaseSync(file);
  db.prepare(
    "UPDATE archive_subscriptions SET budget=? WHERE name='observer'",
  ).run(size * 4);
  for (const row of rows) h.ingest(row);
  expect(h.read().records.map((row) => row.id)).toEqual(
    rows
      .slice(1)
      .reverse()
      .map((row) => row.id),
  );
  expect(h.settings().bytes).toBe(size * 4);
  db.close();
  h.store.close();
});

test("clear reclaims every free page from a multi-megabyte archive", () => {
  const file = path(),
    h = harness(file),
    db = new DatabaseSync(file);
  try {
    // Reclamation exercises stored pages, not 200 fresh-ingest crypto checks and
    // commits. Seed the same multi-megabyte footprint in one transaction; the
    // ingest/decoding tests above retain real signed, encrypted envelopes.
    const insert = db.prepare(
      "INSERT INTO archive_events(viewer,community,subscription,id,agent,kind,created,received,bytes,envelope) VALUES (?,?,'observer',?,?,24200,?,?,16000,zeroblob(16000))",
    );
    const now = Math.floor(Date.now() / 1000);
    db.exec("BEGIN");
    for (let i = 0; i < 200; i++)
      insert.run(
        viewer,
        community,
        i.toString(16).padStart(64, "0"),
        sender,
        now,
        now,
      );
    db.exec("COMMIT; PRAGMA wal_checkpoint(TRUNCATE)");
    const before = statSync(file).size;
    expect(before).toBeGreaterThanOrEqual(200 * 16000);
    expect(h.settings().bytes).toBe(200 * 16000);
    h.request({ action: "clear", kind: 24200 });
    expect(statSync(file).size).toBeLessThan(before);
    expect(statSync(`${file}-wal`).size).toBe(0);
    expect(db.prepare("PRAGMA freelist_count").get().freelist_count).toBe(0);
    expect(h.settings().bytes).toBe(0);
  } finally {
    db.close();
    h.store.close();
  }
});

test("v1 migration preserves encrypted rows, policy settings and CAS revisions", () => {
  const file = path(),
    db = new DatabaseSync(file);
  const schema = readFileSync(
    new URL("../crates/desktop/src/archive/schema.sql", import.meta.url),
    "utf8",
  ).replace(
    "  enabled INTEGER",
    "  scope TEXT NOT NULL CHECK(scope IN ('h','p','e')), value TEXT NOT NULL, kinds TEXT NOT NULL, enabled INTEGER",
  );
  db.exec(schema);
  db.exec("PRAGMA user_version=1");
  db.prepare("INSERT INTO archive_partitions VALUES (?,?,7)").run(
    viewer,
    community,
  );
  for (const [name, kind] of [
    ["observer", 24200],
    ["metrics", 44200],
  ])
    db.prepare(
      "INSERT INTO archive_subscriptions VALUES (?,?,?,'p',?,?,0,7,99999)",
    ).run(viewer, community, name, viewer, JSON.stringify([kind]));
  const row = event(),
    raw = JSON.stringify(row);
  db.prepare(
    "INSERT INTO archive_events(viewer,community,subscription,id,agent,kind,created,received,bytes,envelope) VALUES (?,?,'observer',?,?,24200,?,?,?,?)",
  ).run(
    viewer,
    community,
    row.id,
    row.pubkey,
    row.created_at,
    row.created_at,
    Buffer.byteLength(raw),
    raw,
  );
  const h = harness(file);
  expect(h.settings()).toMatchObject({
    observer: false,
    metrics: false,
    observerDays: 7,
    revision: 7,
    bytes: Buffer.byteLength(raw),
  });
  expect(h.read().records[0].id).toBe(row.id);
  expect(db.prepare("PRAGMA user_version").get().user_version).toBe(2);
  expect(
    db
      .prepare("PRAGMA table_info(archive_subscriptions)")
      .all()
      .map((r) => r.name),
  ).toEqual(["viewer", "community", "name", "enabled", "days", "budget"]);
  h.store.close();
  db.close();
});

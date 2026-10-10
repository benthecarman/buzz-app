// The dev broker owns this database, not the browser. Never store decrypted content.
import { DatabaseSync } from "node:sqlite";
import { readFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { homedir } from "node:os";
import { decodeAgentArchive } from "./agent-observer.mjs";

export const archivePath = () =>
  join(homedir(), ".buzz-foundation", "dev-archive", "events.sqlite3");
export function openArchive(path = archivePath()) {
  if (path !== ":memory:")
    mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  const db = new DatabaseSync(path);
  db.exec("PRAGMA busy_timeout=2000");
  // NONE -> INCREMENTAL is connection-only for existing files without VACUUM.
  // New files need this flag before BEGIN creates the header.
  if (db.prepare("PRAGMA auto_vacuum").get().auto_vacuum === 0)
    db.exec("PRAGMA auto_vacuum=INCREMENTAL");
  db.exec("BEGIN IMMEDIATE");
  try {
    const version = db.prepare("PRAGMA user_version").get().user_version;
    if (version > 2) throw new Error("Archive was created by a newer app");
    if (version === 0) {
      db.exec(
        readFileSync(
          new URL("../crates/desktop/src/archive/schema.sql", import.meta.url),
          "utf8",
        ),
      );
    } else if (version === 1) {
      db.exec(
        readFileSync(
          new URL("../crates/desktop/src/archive/migrate-v2.sql", import.meta.url),
          "utf8",
        ),
      );
    }
    db.exec("PRAGMA user_version=2; COMMIT");
  } catch (error) {
    db.exec("ROLLBACK");
    db.close();
    throw error;
  }
  db.exec("PRAGMA journal_mode=WAL; PRAGMA secure_delete=ON");
  const run = (sql, ...args) => db.prepare(sql).run(...args);
  const get = (sql, ...args) => db.prepare(sql).get(...args);
  const transaction = (action) => {
    db.exec("BEGIN IMMEDIATE");
    try {
      const result = action();
      db.exec("COMMIT");
      return result;
    } catch (error) {
      db.exec("ROLLBACK");
      throw error;
    }
  };
  const seed = (viewer, community) => {
    run(
      "INSERT OR IGNORE INTO archive_partitions(viewer,community) VALUES (?,?)",
      viewer,
      community,
    );
    for (const [name, days, budget] of [
      ["observer", 30, 512 * 1024 * 1024],
      ["metrics", 90, 64 * 1024 * 1024],
    ])
      run(
        "INSERT OR IGNORE INTO archive_subscriptions(viewer,community,name,days,budget) VALUES (?,?,?,?,?)",
        viewer,
        community,
        name,
        days,
        budget,
      );
  };
  const revision = (viewer, community) =>
    get(
      "SELECT revision FROM archive_partitions WHERE viewer=? AND community=?",
      viewer,
      community,
    ).revision;
  let lastPrune = 0;
  const prune = (force = false) => {
    if (!force && Date.now() < lastPrune + 60_000) return;
    run(
      "DELETE FROM archive_events WHERE seq IN (SELECT e.seq FROM archive_events e JOIN archive_subscriptions s ON s.viewer=e.viewer AND s.community=e.community AND s.name=e.subscription WHERE e.created <= ?-s.days*86400)",
      Math.floor(Date.now() / 1000),
    );
    try {
      db.exec("PRAGMA incremental_vacuum(64)");
    } catch {
      /* Expiry committed; optional maintenance retries on the next prune. */
    }
    lastPrune = Date.now();
  };
  const settings = (viewer, community) => {
    // One statement keeps the values and CAS revision on the same snapshot.
    const row = get(
      "SELECT o.enabled AS observer,o.days,m.enabled AS metrics,p.revision,(SELECT COALESCE(SUM(bytes),0) FROM archive_usage WHERE viewer=p.viewer AND community=p.community AND agent='') AS bytes FROM archive_partitions p JOIN archive_subscriptions o USING(viewer,community) JOIN archive_subscriptions m USING(viewer,community) WHERE p.viewer=? AND p.community=? AND o.name='observer' AND m.name='metrics'",
      viewer,
      community,
    );
    return {
      observer: !!row.observer,
      metrics: !!row.metrics,
      observerDays: row.days,
      revision: row.revision,
      location: "broker",
      path,
      bytes: row.bytes,
    };
  };
  const kindCheck = (kind) => {
    if (![24200, 44200].includes(kind))
      throw new Error("Unsupported archive kind");
  };
  return {
    request(viewer, community, input, secret) {
      seed(viewer, community);
      prune();
      switch (input.action) {
        case "settings":
          return settings(viewer, community);
        case "configure": {
          if (
            typeof input.observer !== "boolean" ||
            typeof input.metrics !== "boolean" ||
            !Number.isInteger(input.observerDays) ||
            input.observerDays < 1 ||
            input.observerDays > 90 ||
            !Number.isSafeInteger(input.revision)
          )
            throw new Error("Invalid archive settings");
          transaction(() => {
            const changed = run(
              "UPDATE archive_partitions SET revision=revision+1 WHERE viewer=? AND community=? AND revision=?",
              viewer,
              community,
              input.revision,
            );
            if (changed.changes !== 1)
              throw new Error("Archive settings changed; reload and retry");
            run(
              "UPDATE archive_subscriptions SET enabled=?,days=? WHERE viewer=? AND community=? AND name='observer'",
              Number(input.observer),
              input.observerDays,
              viewer,
              community,
            );
            run(
              "UPDATE archive_subscriptions SET enabled=? WHERE viewer=? AND community=? AND name='metrics'",
              Number(input.metrics),
              viewer,
              community,
            );
          });
          prune(true);
          return settings(viewer, community);
        }
        case "clear":
          if (input.kind !== undefined) kindCheck(input.kind);
          transaction(() => {
            run(
              "UPDATE archive_partitions SET revision=revision+1 WHERE viewer=? AND community=?",
              viewer,
              community,
            );
            run(
              "DELETE FROM archive_events WHERE viewer=? AND community=? AND (? IS NULL OR kind=?)",
              viewer,
              community,
              input.kind ?? null,
              input.kind ?? null,
            );
          });
          // Deletion committed; maintenance must not report a false clear failure.
          try {
            db.exec(
              "PRAGMA incremental_vacuum; PRAGMA wal_checkpoint(TRUNCATE)",
            );
          } catch {
            /* prune retries maintenance */
          }
          return { cleared: true };
        case "read": {
          kindCheck(input.kind);
          if (
            (input.agent !== undefined &&
              !/^[0-9a-f]{64}$/.test(input.agent)) ||
            (input.before !== undefined &&
              (!Number.isSafeInteger(input.before) || input.before <= 0))
          )
            throw new Error("Invalid archive page");
          const rows = db
            .prepare(
              "SELECT seq,received,envelope FROM archive_events WHERE viewer=? AND community=? AND kind=? AND (? IS NULL OR agent=?) AND (? IS NULL OR seq<?) AND created > ?-(SELECT days*86400 FROM archive_subscriptions WHERE viewer=? AND community=? AND name=subscription) ORDER BY seq DESC LIMIT 100",
            )
            .all(
              viewer,
              community,
              input.kind,
              input.agent ?? null,
              input.agent ?? null,
              input.before ?? null,
              input.before ?? null,
              Math.floor(Date.now() / 1000),
              viewer,
              community,
            );
          let skipped = 0;
          const records = rows.flatMap((row) => {
            try {
              return [
                {
                  ...decodeAgentArchive(
                    JSON.parse(row.envelope),
                    secret,
                    viewer,
                    true,
                  ),
                  receivedAt: row.received * 1000,
                },
              ];
            } catch {
              skipped++;
              return [];
            }
          });
          return {
            skipped,
            agents: db
              .prepare(
                "SELECT DISTINCT agent FROM archive_events WHERE viewer=? AND community=? AND kind=? ORDER BY agent LIMIT 256",
              )
              .all(viewer, community, input.kind)
              .map((row) => row.agent),
            records,
            before: rows.length === 100 ? rows.at(-1).seq : null,
            revision: revision(viewer, community),
          };
        }
        default:
          throw new Error("Invalid archive action");
      }
    },
    ingest(viewer, community, event, expectedRevision, secret) {
      decodeAgentArchive(event, secret, viewer);
      seed(viewer, community);
      transaction(() => {
        if (revision(viewer, community) !== expectedRevision)
          throw new Error("Archive revision changed");
        const subscription = event.kind === 24200 ? "observer" : "metrics";
        const rule = get(
          "SELECT enabled,budget FROM archive_subscriptions WHERE viewer=? AND community=? AND name=?",
          viewer,
          community,
          subscription,
        );
        if (!rule.enabled) return;
        const raw = JSON.stringify(event);
        run(
          "INSERT OR IGNORE INTO archive_events(viewer,community,subscription,id,agent,kind,created,received,bytes,envelope) VALUES (?,?,?,?,?,?,?,?,?,?)",
          viewer,
          community,
          subscription,
          event.id,
          event.pubkey,
          event.kind,
          event.created_at,
          Math.floor(Date.now() / 1000),
          Buffer.byteLength(raw),
          raw,
        );
        for (const [agent, cap] of [
          [event.pubkey, rule.budget / 4],
          [null, rule.budget],
        ]) {
          const used = get(
            "SELECT bytes FROM archive_usage WHERE viewer=? AND community=? AND subscription=? AND agent=?",
            viewer,
            community,
            subscription,
            agent ?? "",
          ).bytes;
          if (used <= cap) continue;
          const suffix = agent === null ? "" : " AND agent=?";
          const args = [
            viewer,
            community,
            event.kind,
            ...(agent === null ? [] : [agent]),
          ];
          const rows = db
            .prepare(
              `SELECT seq,bytes FROM archive_events WHERE viewer=? AND community=? AND kind=?${suffix} ORDER BY seq`,
            )
            .iterate(...args);
          let overflow = used - cap,
            cutoff;
          for (const row of rows) {
            cutoff = row.seq;
            overflow -= row.bytes;
            if (overflow <= 0) break;
          }
          if (overflow > 0) throw new Error("Archive usage mismatch");
          run(
            `DELETE FROM archive_events WHERE viewer=? AND community=? AND kind=?${suffix} AND seq<=?`,
            ...args,
            cutoff,
          );
        }
      });
      prune();
    },
    close() {
      db.close();
    },
  };
}

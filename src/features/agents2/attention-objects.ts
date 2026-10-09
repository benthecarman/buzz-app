// One-object reads and writes of an agent's attention, with Janet's scoped
// object contract: a show returns the object and its expected-state token, and
// every write must present the token of the object it replaces. A token is a
// current-state check, not a credential or a revision history. Storage stays the
// Agents2 record; this module only decides what a read shows and whether a
// write may happen.
import {
  interestSlug,
  parseSlug,
  timerSpent,
  timerState,
  validateObject,
  watchSlug,
  type AttentionValue,
  type EventWatch,
  type Interest,
  type TimerWatch,
} from "./attention";
import { setAttention, type AgentRecord } from "./store";

export const ABSENT = "object-v1:absent";
/** The most stored attention an agent can write for itself, so that one agent
 * cannot fill the device storage that every agent's settings share. */
export const AGENT_BYTES = 256 * 1024;
/** The shortest interval an agent can give a timer. Each run is a new Claude
 * turn, so a faster timer only queues turns it cannot run. */
export const MIN_INTERVAL_SECS = 60;
export type ClassifierAvailability = "available" | "unavailable";
/** enabled/disabled for event watches; armed/inactive/spent for timers. */
export type WatchStatus =
  | "enabled"
  | "disabled"
  | "armed"
  | "inactive"
  | "spent";
export type AttentionErrorCode =
  | "conflict"
  | "wrong-scope"
  | "invalid-operation"
  | "not-found"
  | "invalid";
/** A refused attention write; its message starts with the code, as Janet's. */
export class AttentionError extends Error {
  constructor(
    readonly code: AttentionErrorCode,
    detail: string,
  ) {
    super(`${code}: ${detail}`);
  }
}

export type InterestObject = Interest & Readonly<{ id: string }>;
export type EventWatchObject = EventWatch &
  Readonly<{ id: string; status: "enabled" | "disabled" }>;
/** A timer with its schedule: occurrences `used`, and when the next is due. */
export type TimerObject = TimerWatch &
  Readonly<{
    id: string;
    status: "armed" | "inactive" | "spent";
    used: number;
    next_due: number;
  }>;
export type ShownObject = InterestObject | EventWatchObject | TimerObject;
export type Shown = Readonly<{
  result: "found" | "not-found";
  object: ShownObject | null;
  expected_state: string;
}>;
export type WatchSummary = Readonly<{
  id: string;
  interest_id: string;
  name?: string;
  type: "event" | "timer";
  status: WatchStatus;
  /** Whether the watch has a classifier. */
  classified?: true;
}>;
export type WatchFilter = Readonly<{
  interest?: string;
  type?: "event" | "timer";
  status?: WatchStatus;
  /** Case-sensitive, over ids and names. */
  search?: string;
}>;

function timerObject(
  record: AgentRecord,
  id: string,
  timer: TimerWatch,
  now: number,
): TimerObject {
  const state = timerState(timer, record.timers?.[watchSlug(id)], now);
  return {
    id,
    ...timer,
    status: !timer.enabled
      ? "inactive"
      : timerSpent(timer, state, now)
        ? "spent"
        : "armed",
    used: state.used,
    next_due: state.nextDue,
  };
}

/** The object at `slug` as a show presents it, or null. */
export function objectAt(
  record: AgentRecord,
  slug: string,
  now: number,
): ShownObject | null {
  const parsed = parseSlug(slug);
  const value = record.attention[slug]?.value;
  if (!parsed || !value) return null;
  if (value.type === "interest") return { id: parsed.id, ...value };
  if (value.type === "event")
    return {
      id: parsed.id,
      ...value,
      status: value.enabled ? "enabled" : "disabled",
    };
  return timerObject(record, parsed.id, value, now);
}

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === "object")
    return Object.fromEntries(
      Object.keys(value)
        .sort()
        .map((key) => [
          key,
          canonical((value as Record<string, unknown>)[key]),
        ]),
    );
  return value;
}
/** `object-v1:` and the SHA-256 of the canonical object, or ABSENT. */
export async function token(object: ShownObject | null) {
  if (!object) return ABSENT;
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(JSON.stringify(canonical(object))),
  );
  const hex = [...new Uint8Array(digest)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
  return `object-v1:${hex}`;
}

/** Refuses an object outside the caller's Interest, as Janet's scope does. */
function checkScope(
  record: AgentRecord,
  slug: string,
  value: AttentionValue | null,
  interest: string | undefined,
) {
  const parsed = parseSlug(slug);
  if (interest === undefined || !parsed) return;
  const prior = record.attention[slug]?.value;
  const owner =
    parsed.space === "interest"
      ? parsed.id
      : prior && prior.type !== "interest"
        ? prior.interest_id
        : value && value.type !== "interest"
          ? value.interest_id
          : interest;
  if (owner !== interest)
    throw new AttentionError(
      "wrong-scope",
      parsed.space === "interest"
        ? `Interest ${parsed.id} is not ${interest}`
        : `Watch ${parsed.id} belongs to Interest ${owner}`,
    );
}

export async function show(
  record: AgentRecord,
  slug: string,
  now: number,
  interest?: string,
): Promise<Shown> {
  checkScope(record, slug, null, interest);
  const object = objectAt(record, slug, now);
  return {
    result: object ? "found" : "not-found",
    object,
    expected_state: await token(object),
  };
}

/** Interest ids, in order; `search` is case-sensitive. */
export function interestIds(record: AgentRecord, search = "") {
  return Object.values(record.attention)
    .filter((object) => object.value.type === "interest")
    .map((object) => parseSlug(object.slug)?.id ?? "")
    .filter((id) => !!id && id.includes(search))
    .sort();
}

export function watchSummaries(
  record: AgentRecord,
  filter: WatchFilter,
  now: number,
): WatchSummary[] {
  const search = filter.search ?? "";
  return Object.values(record.attention)
    .flatMap((object): WatchSummary[] => {
      const id = parseSlug(object.slug)?.id;
      const value = object.value;
      if (!id || value.type === "interest") return [];
      const status =
        value.type === "event"
          ? value.enabled
            ? "enabled"
            : "disabled"
          : timerObject(record, id, value, now).status;
      return [
        {
          id,
          interest_id: value.interest_id,
          ...(value.type === "event" && value.name ? { name: value.name } : {}),
          type: value.type,
          status,
          ...(value.type === "event" && value.classifier
            ? { classified: true as const }
            : {}),
        },
      ];
    })
    .filter(
      (watch) =>
        (!filter.interest || watch.interest_id === filter.interest) &&
        (!filter.type || watch.type === filter.type) &&
        (!filter.status || watch.status === filter.status) &&
        (watch.id.includes(search) || !!watch.name?.includes(search)),
    )
    .sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
}

export type WriteOptions = Readonly<{
  /** The token from the latest show of this object; ABSENT to create. */
  expected: string;
  /** The Interest the caller works for. A watch of another, or another
   * Interest, is `wrong-scope`. */
  interest?: string;
}>;

/** The record after one checked write: replaces (or with `null` removes) the
 * object at `slug`. Throws an AttentionError when the write is refused. */
export async function checkedWrite(
  record: AgentRecord,
  slug: string,
  value: AttentionValue | null,
  { expected, interest }: WriteOptions,
  now: number,
): Promise<AgentRecord> {
  const parsed = parseSlug(slug);
  if (!parsed) throw new AttentionError("invalid", `Invalid slug: ${slug}`);
  if (!expected?.startsWith("object-v1:"))
    throw new AttentionError(
      "invalid-operation",
      "Every write needs the expected_state of a show",
    );
  const problem = value && validateObject(slug, value);
  if (problem) throw new AttentionError("invalid", problem);
  const was = record.attention[slug]?.value;
  // An interval the owner set stays editable; the agent cannot set a new one.
  if (
    value?.type === "timer" &&
    value.interval_secs < MIN_INTERVAL_SECS &&
    !(was?.type === "timer" && was.interval_secs === value.interval_secs)
  )
    throw new AttentionError(
      "invalid",
      `interval_secs must be at least ${MIN_INTERVAL_SECS}`,
    );
  // Stored by a newer app, or invalid: this app cannot show it, so it cannot
  // check a token against it, and a write would replace what it never read.
  if (record.skipped?.[slug])
    throw new AttentionError(
      "invalid-operation",
      `${slug} is stored in a form this app cannot read; your owner can fix or remove it`,
    );
  const prior = record.attention[slug]?.value;
  checkScope(record, slug, value, interest);
  if ((await token(objectAt(record, slug, now))) !== expected)
    throw new AttentionError(
      "conflict",
      prior
        ? `${slug} changed since it was read; show it again`
        : `${slug} does not exist; show it again`,
    );
  if (value && (value.type === "interest") !== (parsed.space === "interest"))
    throw new AttentionError(
      "invalid",
      `${slug} cannot hold a ${value.type === "interest" ? "Interest" : "watch"}`,
    );
  if (
    prior &&
    value &&
    prior.type !== "interest" &&
    value.type !== "interest" &&
    prior.type !== value.type
  )
    throw new AttentionError(
      "invalid-operation",
      `Watch ${parsed.id} is ${prior.type === "event" ? "an event watch" : "a timer"}; its type cannot change`,
    );
  if (
    prior &&
    value &&
    prior.type !== "interest" &&
    value.type !== "interest" &&
    prior.interest_id !== value.interest_id
  )
    throw new AttentionError(
      "invalid-operation",
      "A watch cannot move to another Interest",
    );
  if (
    value &&
    value.type !== "interest" &&
    !record.attention[interestSlug(value.interest_id)]
  )
    throw new AttentionError(
      "not-found",
      `Interest ${value.interest_id} does not exist; set it first`,
    );
  if (!value && parsed.space === "interest") {
    const users = watchSummaries(record, { interest: parsed.id }, now);
    if (users.length)
      throw new AttentionError(
        "invalid-operation",
        `Remove its watches first: ${users.map((watch) => watch.id).join(", ")}`,
      );
  }
  let after: AgentRecord;
  try {
    after = setAttention(record, slug, value, now);
  } catch (error) {
    throw new AttentionError(
      "invalid",
      error instanceof Error ? error.message : String(error),
    );
  }
  if (value && storedBytes(after) > AGENT_BYTES)
    throw new AttentionError(
      "invalid",
      `Your attention would take more than ${AGENT_BYTES / 1024} KB; remove or shorten other objects first`,
    );
  return after;
}

/** The bytes the record's attention takes in storage, unreadable objects too. */
function storedBytes(record: AgentRecord) {
  return new TextEncoder().encode(
    JSON.stringify([record.attention, record.skipped ?? {}]),
  ).length;
}

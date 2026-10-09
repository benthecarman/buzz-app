// The app's editor for an agent's `agent-attention/v1` objects. Every agent type is
// woken through them, so the app edits them once instead of each plugin. Watches
// and timers sit under the Interest they serve; raw JSON covers what the forms
// leave out (tags, classifiers). Validation is the spec's, at save.
import { useState } from "react";
import { formatItemTimestamp } from "../../shared/datetime";
import {
  AtIcon,
  DotsThreeIcon,
  EyeIcon,
  PencilSimpleIcon,
  PlusIcon,
  TimerIcon,
  TrashIcon,
  WarningCircleIcon,
} from "../../shared/design-system/icons";
import { Button } from "../../shared/design-system/ui/Button";
import { Checkbox } from "../../shared/design-system/ui/Checkbox";
import { Field } from "../../shared/design-system/ui/Field";
import { IconButton } from "../../shared/design-system/ui/IconButton";
import { Input } from "../../shared/design-system/ui/Input";
import {
  MenuIcon,
  MenuItem,
  MenuPopup,
  MenuRoot,
  MenuTrigger,
} from "../../shared/design-system/ui/Menu";
import { PreferenceRow } from "../../shared/design-system/ui/PreferenceRow";
import { Select } from "../../shared/design-system/ui/Select";
import { SettingsGroup } from "../../shared/design-system/ui/SettingsGroup";
import { Switch } from "../../shared/design-system/ui/Switch";
import { Textarea } from "../../shared/design-system/ui/Textarea";
import {
  attentionOf,
  compileFilter,
  describeWatch,
  interestTitle,
  interval,
  parseSlug,
  plural,
  timerSpent,
  timerState,
  UNITS,
  validateObject,
  type AttentionObject,
  type AttentionValue,
  type ChannelChoice,
  type EventWatch,
  type TimerWatch,
  type Watch,
  type WatchObject,
} from "./attention";
import type { Agent, AgentChange } from "./service";
import type { SkippedObject } from "./store";

/** The app's own writer: any field, including attention. */
type Save = (change: AgentChange) => Promise<void>;

function unnamed(watch: EventWatch): EventWatch {
  const { name: _, ...rest } = watch;
  return rest;
}

const watchIcon = (watch: Watch) =>
  watch.type === "timer" ? (
    <TimerIcon size={14} aria-hidden="true" />
  ) : (
    <EyeIcon size={14} aria-hidden="true" />
  );

/** Read-only: what wakes the agent, one line each. For glances like the peek. */
export function AttentionSummary({
  agent,
  channels = [],
}: {
  agent: Agent;
  channels?: readonly ChannelChoice[];
}) {
  const { watches } = attentionOf(agent.attention);
  const on = watches.filter((object) => object.value.enabled);
  const paused = watches.length - on.length;
  return (
    <ul className="m-0 grid list-none gap-1.5 p-0 text-body-sm">
      <li className="flex items-center gap-2">
        <AtIcon size={14} aria-hidden="true" /> Mentions and replies
      </li>
      {on.map((object) => (
        <li key={object.slug} className="flex min-w-0 items-center gap-2">
          {watchIcon(object.value)}
          <span className="truncate">
            {describeWatch(object.value, channels)}
          </span>
        </li>
      ))}
      {paused > 0 && (
        <li className="text-secondary">{plural(paused, "paused watch")}</li>
      )}
    </ul>
  );
}

const message = (error: unknown) =>
  error instanceof Error ? error.message : String(error);
function useAction() {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const run = async (action: () => Promise<void>) => {
    setPending(true);
    setError("");
    try {
      await action();
      return true;
    } catch (reason) {
      setError(message(reason));
      return false;
    } finally {
      setPending(false);
    }
  };
  return { pending, error, setError, run };
}
const Problem = ({ error }: { error: string }) =>
  error ? (
    <p role="alert" className="m-0 text-body-sm text-danger">
      {error}
    </p>
  ) : null;

export function AttentionPanel({
  agent,
  save,
  channels = [],
}: {
  agent: Agent;
  save: Save;
  channels?: readonly ChannelChoice[];
}) {
  const { interests, watches, orphans } = attentionOf(agent.attention);
  const [adding, setAdding] = useState(false);
  const switched = useAction();
  const skipped = Object.values(agent.skipped);
  const row = (object: WatchObject) => (
    <WatchRow
      key={`${object.slug}:${object.modifiedAt}`}
      object={object}
      agent={agent}
      channels={channels}
      save={save}
    />
  );
  return (
    <div className="grid content-start gap-6">
      <p className="m-0 text-body-sm text-secondary">
        What wakes this agent. Mentions and replies always do. For anything
        else, add an Interest: something it looks after, with instructions. The
        watches and timers under an Interest decide when it wakes for it.
      </p>
      <SettingsGroup>
        <PreferenceRow
          icon={<AtIcon size={16} />}
          title="Mentions and replies"
          subtitle="Messages that mention it or reply to it. Always on."
        />
        <PreferenceRow
          icon={<EyeIcon size={16} />}
          title="Attention"
          subtitle={
            <>
              Lets the agent add and change its own Interests, watches and
              timers, and wakes it for them. Off, nothing below wakes it, and it
              is not offered the tools.
              <Problem error={switched.error} />
            </>
          }
          trailing={
            <Switch
              aria-label="Attention on"
              checked={agent.attentionEnabled}
              disabled={switched.pending}
              onCheckedChange={(attentionEnabled) =>
                void switched.run(() => save({ attentionEnabled }))
              }
            />
          }
        />
      </SettingsGroup>
      {interests.map((object) => {
        const id = parseSlug(object.slug)?.id ?? object.slug;
        return (
          <InterestGroup
            key={object.slug}
            id={id}
            object={object}
            watches={watches.filter((watch) => watch.value.interest_id === id)}
            agent={agent}
            channels={channels}
            save={save}
            row={row}
          />
        );
      })}
      {orphans.length > 0 && (
        <section className="grid gap-2" aria-label="Without an interest">
          <div className="grid gap-0.5">
            <h4 className="m-0 text-label">Without an interest</h4>
            <p className="m-0 text-body-sm text-secondary">
              These name an Interest that does not exist, so they wake the agent
              without instructions. Edit the JSON to set its interest_id, or
              remove it.
            </p>
          </div>
          <SettingsGroup>{orphans.map(row)}</SettingsGroup>
        </section>
      )}
      {skipped.length > 0 && (
        <section className="grid gap-2" aria-label="Skipped">
          <div className="grid gap-0.5">
            <h4 className="m-0 text-label">Skipped</h4>
            <p className="m-0 text-body-sm text-secondary">
              These are saved but never wake the agent, because they are invalid
              or over a limit. Fix the JSON, or remove them.
            </p>
          </div>
          <SettingsGroup>
            {skipped.map((object) => (
              <SkippedRow
                key={`${object.slug}:${object.modifiedAt}`}
                object={object}
                save={save}
              />
            ))}
          </SettingsGroup>
        </section>
      )}
      {adding ? (
        <NewInterest
          agent={agent}
          save={save}
          onDone={() => setAdding(false)}
        />
      ) : (
        <div>
          <Button size="compact" onClick={() => setAdding(true)}>
            <PlusIcon size={14} aria-hidden="true" /> Add interest
          </Button>
        </div>
      )}
    </div>
  );
}

function InterestGroup({
  id,
  object,
  watches,
  agent,
  channels,
  save,
  row,
}: {
  id: string;
  object: AttentionObject;
  watches: readonly WatchObject[];
  agent: Agent;
  channels: readonly ChannelChoice[];
  save: Save;
  row(object: WatchObject): React.ReactNode;
}) {
  const instructions =
    object.value.type === "interest" ? object.value.instructions : "";
  const title = interestTitle(id);
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState(instructions);
  const [draft, setDraft] = useState<"event" | "timer">();
  const action = useAction();
  const remove = () =>
    action.run(() => save({ attention: { [object.slug]: null } }));
  return (
    <section className="grid gap-2" aria-label={title}>
      <div className="flex items-center justify-between gap-2">
        <h4 className="m-0 text-label">{title}</h4>
        <RowMenu
          label={title}
          onEdit={() => {
            setText(instructions);
            setEditing(true);
          }}
          // The spec keeps an Interest while anything still uses it.
          removeLabel={
            watches.length ? "Remove (first remove what uses it)" : "Remove"
          }
          removeDisabled={watches.length > 0}
          onRemove={() => void remove()}
        />
      </div>
      {editing ? (
        <div className="grid gap-2">
          <Field label={`${title} instructions`} labelVisibility="hidden">
            <Textarea
              rows={3}
              value={text}
              onChange={(event) => setText(event.target.value)}
            />
          </Field>
          <FormActions
            pending={action.pending}
            submit="Save"
            onSubmit={() =>
              void action
                .run(() =>
                  save({
                    attention: {
                      [object.slug]: { type: "interest", instructions: text },
                    },
                  }),
                )
                .then((ok) => ok && setEditing(false))
            }
            onCancel={() => setEditing(false)}
          />
        </div>
      ) : (
        <p className="m-0 whitespace-pre-wrap text-body-sm text-secondary">
          {instructions}
        </p>
      )}
      <Problem error={action.error} />
      {watches.length > 0 ? (
        <SettingsGroup>{watches.map(row)}</SettingsGroup>
      ) : (
        !draft && (
          <p className="m-0 text-body-sm text-secondary">
            Nothing wakes it for this yet. Add a watch to wake it on events, or
            a timer to wake it on a schedule.
          </p>
        )
      )}
      {draft ? (
        <WatchForm
          kind={draft}
          interest={id}
          agent={agent}
          channels={channels}
          save={save}
          onDone={() => setDraft(undefined)}
        />
      ) : (
        <div className="flex gap-2">
          <Button size="compact" onClick={() => setDraft("event")}>
            <EyeIcon size={14} aria-hidden="true" /> Add watch
          </Button>
          <Button size="compact" onClick={() => setDraft("timer")}>
            <TimerIcon size={14} aria-hidden="true" /> Add timer
          </Button>
        </div>
      )}
    </section>
  );
}

function RowMenu({
  label,
  onEdit,
  editLabel = "Edit",
  removeLabel = "Remove",
  removeDisabled = false,
  onRemove,
  extra,
}: {
  label: string;
  onEdit(): void;
  editLabel?: string;
  removeLabel?: string;
  removeDisabled?: boolean;
  onRemove(): void;
  extra?: React.ReactNode;
}) {
  return (
    <MenuRoot>
      <MenuTrigger
        render={
          <IconButton
            aria-label={`Actions for ${label}`}
            size="sm"
            icon={<DotsThreeIcon size={18} aria-hidden="true" />}
          />
        }
      />
      <MenuPopup size="compact" align="end">
        <MenuItem onClick={onEdit}>
          <MenuIcon>
            <PencilSimpleIcon size={14} />
          </MenuIcon>
          {editLabel}
        </MenuItem>
        {extra}
        <MenuItem tone="danger" disabled={removeDisabled} onClick={onRemove}>
          <MenuIcon>
            <TrashIcon size={14} />
          </MenuIcon>
          {removeLabel}
        </MenuItem>
      </MenuPopup>
    </MenuRoot>
  );
}

/** Where a timer's schedule stands, for its row. */
function timerStatus(agent: Agent, slug: string, timer: TimerWatch) {
  const at = now();
  const state = timerState(timer, agent.timers[slug], at);
  const runs =
    timer.max_occurrences !== null
      ? `ran ${state.used} of ${plural(timer.max_occurrences, "time")}`
      : state.used > 0 && `ran ${plural(state.used, "time")}`;
  if (timerSpent(timer, state, at))
    return {
      spent: true,
      text:
        timer.expires_at !== null && at >= timer.expires_at
          ? "Ended"
          : `Done, ${runs}`,
    };
  return {
    spent: false,
    text: [
      timer.enabled &&
        `next ${formatItemTimestamp(Math.max(state.nextDue, at), { withTime: true })}`,
      runs,
      timer.expires_at !== null &&
        `until ${formatItemTimestamp(timer.expires_at, { withTime: true })}`,
    ]
      .filter(Boolean)
      .join(", "),
  };
}

function WatchRow({
  object,
  agent,
  channels,
  save,
}: {
  object: WatchObject;
  agent: Agent;
  channels: readonly ChannelChoice[];
  save: Save;
}) {
  const { value, slug } = object;
  const [editing, setEditing] = useState(false);
  const action = useAction();
  const title = describeWatch(value, channels);
  const put = (next: AttentionValue | null) =>
    action.run(() => save({ attention: { [slug]: next } }));
  const status =
    value.type === "timer" ? timerStatus(agent, slug, value) : undefined;
  const detail =
    value.type === "timer"
      ? [`“${value.prompt}”`, status?.text]
      : [
          value.name && describeWatch(unnamed(value), channels),
          value.filter && `when ${value.filter}`,
          value.classifier &&
            "has a classifier, which this app cannot run yet, so it matches without it",
        ];
  return (
    <>
      <PreferenceRow
        icon={
          value.type === "timer" ? (
            <TimerIcon size={16} />
          ) : (
            <EyeIcon size={16} />
          )
        }
        title={title}
        disabled={!value.enabled}
        subtitle={
          detail.some(Boolean) || action.error ? (
            <>
              {detail.filter(Boolean).join(" · ")}
              <Problem error={action.error} />
            </>
          ) : undefined
        }
        trailing={
          <span className="flex items-center gap-1">
            <Switch
              aria-label={`${title} on`}
              checked={value.enabled}
              disabled={action.pending}
              onCheckedChange={(enabled) => void put({ ...value, enabled })}
            />
            <RowMenu
              label={title}
              editLabel="Edit JSON"
              onEdit={() => setEditing(true)}
              onRemove={() => void put(null)}
              extra={
                value.type === "timer" &&
                status?.spent && (
                  // A new armed_at is a new schedule, with nothing used yet.
                  <MenuItem
                    onClick={() =>
                      void put({
                        ...value,
                        enabled: true,
                        armed_at: now(),
                        expires_at:
                          value.expires_at !== null && value.expires_at <= now()
                            ? null
                            : value.expires_at,
                      })
                    }
                  >
                    <MenuIcon>
                      <TimerIcon size={14} />
                    </MenuIcon>
                    Run again
                  </MenuItem>
                )
              }
            />
          </span>
        }
      />
      {editing && (
        <JsonEditor
          slug={slug}
          value={value}
          save={save}
          onDone={() => setEditing(false)}
        />
      )}
    </>
  );
}

/** A stored object the reader skipped: its problem, and the way to fix it. */
function SkippedRow({ object, save }: { object: SkippedObject; save: Save }) {
  const [editing, setEditing] = useState(false);
  const action = useAction();
  return (
    <>
      <PreferenceRow
        icon={<WarningCircleIcon size={16} />}
        title={object.slug}
        subtitle={
          <>
            {object.problem}
            <Problem error={action.error} />
          </>
        }
        trailing={
          <RowMenu
            label={object.slug}
            editLabel="Edit JSON"
            onEdit={() => setEditing(true)}
            onRemove={() =>
              void action.run(() =>
                save({ attention: { [object.slug]: null } }),
              )
            }
          />
        }
      />
      {editing && (
        <JsonEditor
          slug={object.slug}
          value={object.value}
          save={save}
          onDone={() => setEditing(false)}
        />
      )}
    </>
  );
}

function FormActions({
  pending,
  submit,
  onSubmit,
  onCancel,
}: {
  pending: boolean;
  submit: string;
  onSubmit(): void;
  onCancel(): void;
}) {
  return (
    <div className="flex gap-2">
      <Button
        size="compact"
        variant="primary"
        loading={pending}
        onClick={onSubmit}
      >
        {submit}
      </Button>
      <Button size="compact" disabled={pending} onClick={onCancel}>
        Cancel
      </Button>
    </div>
  );
}

/** Every field of one object, for what the forms leave out. */
function JsonEditor({
  slug,
  value,
  save,
  onDone,
}: {
  slug: string;
  value: unknown;
  save: Save;
  onDone(): void;
}) {
  const [text, setText] = useState(JSON.stringify(value ?? {}, null, 2));
  const action = useAction();
  const submit = () => {
    let next: unknown;
    try {
      next = JSON.parse(text);
    } catch {
      action.setError("Enter a JSON object");
      return;
    }
    const problem = validateObject(slug, next);
    if (problem) action.setError(problem);
    else
      void action
        .run(() => save({ attention: { [slug]: next as AttentionValue } }))
        .then((ok) => ok && onDone());
  };
  return (
    <div className="grid gap-2 p-3">
      <Field label={slug} description="agent-attention/v1">
        <Textarea
          variant="code"
          rows={10}
          value={text}
          onChange={(event) => setText(event.target.value)}
        />
      </Field>
      <Problem error={action.error} />
      <FormActions
        pending={action.pending}
        submit="Save"
        onSubmit={submit}
        onCancel={onDone}
      />
    </div>
  );
}

// Slug ids are printable ASCII without spaces, unique within the agent.
function freeSlug(space: "interest" | "watch", base: string, agent: Agent) {
  const stem =
    base
      .trim()
      .toLowerCase()
      .replace(/[^!-~]+/g, "-")
      .slice(0, 56) || space;
  // A skipped object still owns its slug until it is fixed or removed.
  const taken = (slug: string) =>
    slug in agent.attention || slug in agent.skipped;
  let id = stem;
  for (let n = 2; taken(`${space}/${id}`); n++) id = `${stem}-${n}`;
  return `${space}/${id}`;
}
const now = () => Math.floor(Date.now() / 1000);
const straightQuotes = (text: string) =>
  text.replace(/[\u201C\u201D\u201E\u201F\u2033]/g, '"');

function NewInterest({
  agent,
  save,
  onDone,
}: {
  agent: Agent;
  save: Save;
  onDone(): void;
}) {
  const [name, setName] = useState("");
  const [instructions, setInstructions] = useState("");
  const action = useAction();
  const submit = () => {
    const slug = freeSlug("interest", name, agent);
    const value = { type: "interest", instructions } as const;
    const problem = !name.trim()
      ? "Name the interest"
      : validateObject(slug, value);
    if (problem) action.setError(problem);
    else
      void action
        .run(() => save({ attention: { [slug]: value } }))
        .then((ok) => ok && onDone());
  };
  return (
    <section
      className="grid gap-3 rounded-container bg-surface-inset p-4"
      aria-label="New interest"
    >
      <Field
        label="Interest"
        description="Something it looks after, e.g. Releases"
      >
        <Input value={name} onChange={(event) => setName(event.target.value)} />
      </Field>
      <Field label="Instructions">
        <Textarea
          rows={3}
          value={instructions}
          placeholder="Keep the release checklist current and flag blockers."
          onChange={(event) => setInstructions(event.target.value)}
        />
      </Field>
      <Problem error={action.error} />
      <FormActions
        pending={action.pending}
        submit="Add interest"
        onSubmit={submit}
        onCancel={onDone}
      />
    </section>
  );
}

const KINDS = {
  messages: [9],
  any: [],
} as const;
const REPLIES = { any: "", only: "is_reply", none: "!is_reply" } as const;

/** Adds one watch or timer for `interest`. Editing an existing one is its JSON,
 * so the form never has to read every field of the schema back in. */
function WatchForm({
  kind,
  interest,
  agent,
  channels,
  save,
  onDone,
}: {
  kind: "event" | "timer";
  interest: string;
  agent: Agent;
  channels: readonly ChannelChoice[];
  save: Save;
  onDone(): void;
}) {
  const [name, setName] = useState("");
  const [scope, setScope] = useState<string[] | "all">("all");
  const [kinds, setKinds] = useState<keyof typeof KINDS>("messages");
  const [exactly, setExactly] = useState("");
  const [replies, setReplies] = useState<keyof typeof REPLIES>("any");
  const [advanced, setAdvanced] = useState("");
  const [prompt, setPrompt] = useState("");
  const [count, setCount] = useState("1");
  const [unit, setUnit] = useState(String(interval(3_600).size));
  const [limit, setLimit] = useState("");
  const [until, setUntil] = useState("");
  const action = useAction();
  let filterProblem = "";
  if (advanced.trim())
    try {
      compileFilter(advanced.trim());
    } catch (error) {
      filterProblem = message(error);
    }
  // The simple fields and the advanced expression, all of which must hold.
  const filter = [
    exactly && `content == ${JSON.stringify(exactly)}`,
    REPLIES[replies],
    advanced.trim() &&
      (exactly || REPLIES[replies] ? `(${advanced.trim()})` : advanced.trim()),
  ]
    .filter(Boolean)
    .join(" && ");
  const expires = until ? Math.floor(new Date(until).getTime() / 1000) : null;
  const build = (): AttentionValue =>
    kind === "event"
      ? {
          type: "event",
          interest_id: interest,
          enabled: true,
          since: now(),
          channels: scope,
          kinds: KINDS[kinds],
          ...(name.trim() ? { name: name.trim() } : {}),
          ...(filter ? { filter } : {}),
        }
      : {
          type: "timer",
          interest_id: interest,
          prompt,
          enabled: true,
          interval_secs: Math.round(Number(count) * Number(unit)),
          armed_at: now(),
          max_occurrences: limit.trim() ? Number(limit) : null,
          expires_at: expires,
        };
  const submit = () => {
    const slug = freeSlug(
      "watch",
      kind === "timer" ? "timer" : name || "messages",
      agent,
    );
    const value = build();
    const problem =
      filterProblem ||
      (Array.isArray(scope) && !scope.length
        ? "Choose at least one channel"
        : expires !== null && (Number.isNaN(expires) || expires <= now())
          ? "Choose an end time in the future"
          : validateObject(slug, value));
    // A filter problem is already shown under the field.
    if (problem) {
      if (!filterProblem) action.setError(problem);
    } else
      void action
        .run(() => save({ attention: { [slug]: value } }))
        .then((ok) => ok && onDone());
  };
  const title = `New ${kind === "timer" ? "timer" : "watch"}`;
  return (
    <section
      className="grid gap-3 rounded-container bg-surface-inset p-4"
      aria-label={title}
    >
      {kind === "event" ? (
        <>
          <Field label="Name (optional)">
            <Input
              value={name}
              placeholder="Release questions"
              onChange={(event) => setName(event.target.value)}
            />
          </Field>
          <Select
            label="Wake on"
            variant="field"
            value={kinds}
            onValueChange={(value) => setKinds(value as keyof typeof KINDS)}
            groups={[
              {
                label: "Events",
                options: [
                  { value: "messages", label: "Messages" },
                  { value: "any", label: "Any event" },
                ],
              },
            ]}
          />
          <Select
            label="In"
            variant="field"
            value={scope === "all" ? "all" : "some"}
            onValueChange={(value) =>
              setScope(value === "all" ? "all" : scope === "all" ? [] : scope)
            }
            groups={[
              {
                label: "Channels",
                options: [
                  { value: "all", label: "Every channel it can read" },
                  { value: "some", label: "Chosen channels" },
                ],
              },
            ]}
          />
          {scope !== "all" && (
            <fieldset className="m-0 grid max-h-48 gap-1 overflow-auto border-0 p-0">
              <legend className="sr-only">Channels</legend>
              {channels.map((channel) => (
                <Checkbox
                  key={channel.id}
                  label={`#${channel.name}`}
                  checked={scope.includes(channel.id)}
                  onCheckedChange={(checked) =>
                    setScope(
                      checked
                        ? [...scope, channel.id]
                        : scope.filter((id) => id !== channel.id),
                    )
                  }
                />
              ))}
              {!channels.length && (
                <p className="m-0 text-body-sm text-secondary">
                  No channels to choose from yet.
                </p>
              )}
            </fieldset>
          )}
          <Field
            label="Text is exactly (optional)"
            description="The whole message, not a part of it"
          >
            <Input
              value={exactly}
              placeholder="deploy"
              spellCheck={false}
              onChange={(event) => setExactly(event.target.value)}
            />
          </Field>
          <Select
            label="Replies"
            variant="field"
            value={replies}
            onValueChange={(value) => setReplies(value as keyof typeof REPLIES)}
            groups={[
              {
                label: "Replies",
                options: [
                  { value: "any", label: "Replies or not" },
                  { value: "only", label: "Only replies" },
                  { value: "none", label: "Not replies" },
                ],
              },
            ]}
          />
          <details className="grid gap-2">
            <summary className="cursor-pointer text-body-sm text-secondary">
              Advanced filter
            </summary>
            <Field
              label="Filter expression (optional)"
              description={
                filterProblem ||
                'e.g. author == "<hex key>" || content == "help". Use &&, ||, ! and ( ).'
              }
            >
              <Input
                value={advanced}
                spellCheck={false}
                autoCorrect="off"
                autoCapitalize="off"
                aria-invalid={!!filterProblem || undefined}
                // macOS turns typed quotes into curly ones; the grammar only takes ".
                onChange={(event) =>
                  setAdvanced(straightQuotes(event.target.value))
                }
              />
            </Field>
          </details>
        </>
      ) : (
        <>
          <Field label="Prompt">
            <Textarea
              rows={2}
              value={prompt}
              placeholder="Post a summary of open questions."
              onChange={(event) => setPrompt(event.target.value)}
            />
          </Field>
          <div className="grid grid-cols-[6rem_1fr] items-end gap-2">
            <Field label="Every">
              <Input
                type="number"
                min={1}
                value={count}
                onChange={(event) => setCount(event.target.value)}
              />
            </Field>
            <Select
              label="Unit"
              variant="compact"
              value={unit}
              onValueChange={setUnit}
              groups={[
                {
                  label: "Units",
                  options: UNITS.slice(0, 3).map(([label, size]) => ({
                    value: String(size),
                    label: `${label}s`,
                  })),
                },
              ]}
            />
          </div>
          <Field label="Stop after (optional)" description="Times it runs">
            <Input
              type="number"
              min={1}
              value={limit}
              onChange={(event) => setLimit(event.target.value)}
            />
          </Field>
          <Field label="Until (optional)" description="It stops at this time">
            <Input
              type="datetime-local"
              value={until}
              onChange={(event) => setUntil(event.target.value)}
            />
          </Field>
        </>
      )}
      <Problem error={action.error} />
      <FormActions
        pending={action.pending}
        submit={kind === "timer" ? "Add timer" : "Add watch"}
        onSubmit={submit}
        onCancel={onDone}
      />
    </section>
  );
}

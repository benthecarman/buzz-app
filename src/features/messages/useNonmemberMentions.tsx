import { useRef, useState } from "react";
import type { RelaySession } from "../relay/session";
import { canAddMembers } from "../channel-members/members";
import type { MentionRecipient } from "./mention-draft";
import { Dialog } from "../../shared/design-system/ui/Dialog";
import { Button } from "../../shared/design-system/ui/Button";

type Pending = {
  people: readonly MentionRecipient[];
  signal: AbortSignal;
  current(): boolean;
  finish(references: readonly string[] | null): void;
  busy: boolean;
  error?: string;
};

/** One consent decision for a captured send. The session owns durable additions. */
export function useNonmemberMentions(
  session: RelaySession,
  channelId: string,
  restoreFocus: () => void,
) {
  const [pending, setPending] = useState<Pending>();
  const adding = useRef(false);
  const safeAction = useRef<HTMLButtonElement>(null);
  const inviteAction = useRef<HTMLButtonElement>(null);
  const channel = session.channels
    .list()
    .channels.find((item) => item.id === channelId);
  const canAdd = !!channel && canAddMembers(session, channel);
  async function add() {
    if (!pending || adding.current || !canAdd) return;
    adding.current = true;
    const request = pending;
    const check = () => {
      request.signal.throwIfAborted();
      if (!request.current())
        throw new Error("The draft changed. Close this dialog and send again.");
    };
    setPending({ ...request, busy: true });
    try {
      for (const person of request.people) {
        check();
        await session.memberAdditions.add(channelId, person.pubkey, undefined, {
          startAgent: false,
        });
      }
      check();
      request.finish([]);
    } catch (reason) {
      if (!request.signal.aborted)
        setPending({
          ...request,
          busy: false,
          error: reason instanceof Error ? reason.message : String(reason),
        });
    } finally {
      adding.current = false;
    }
  }
  return {
    prepare(
      people: readonly MentionRecipient[],
      signal: AbortSignal,
      current: () => boolean,
    ) {
      signal.throwIfAborted();
      return new Promise<readonly string[] | null>((resolve) => {
        const finish = (references: readonly string[] | null) => {
          signal.removeEventListener("abort", abort);
          setPending(undefined);
          resolve(references);
        };
        const abort = () => finish(null);
        signal.addEventListener("abort", abort, { once: true });
        setPending({ people, signal, current, finish, busy: false });
      });
    },
    dialog: (
      <Dialog
        open={!!pending}
        onOpenChange={(open) => {
          if (!open) pending?.finish(null);
        }}
        title="Mention people outside this channel?"
        description={pending && describe(pending.people, canAdd)}
        preventClose={!!pending?.busy}
        initialFocus={canAdd ? inviteAction : safeAction}
        finalFocus={() => {
          restoreFocus();
          return false;
        }}
        actions={
          <>
            <Button
              ref={safeAction}
              type="button"
              disabled={pending?.busy}
              onClick={() =>
                pending?.finish(pending.people.map((person) => person.pubkey))
              }
            >
              {canAdd ? "Do nothing" : "Send anyway"}
            </Button>
            {canAdd && (
              <Button
                ref={inviteAction}
                type="button"
                variant="prominent"
                disabled={pending?.busy}
                onClick={() => void add()}
              >
                {pending?.busy ? "Inviting…" : "Invite"}
              </Button>
            )}
          </>
        }
      >
        {pending?.error ? (
          <p role="alert">
            {pending.error} Your draft is kept. Some people may already have
            been added.
          </p>
        ) : null}
      </Dialog>
    ),
  };
}

/** Match block/buzz desktop: name the people, then the available outcome. */
function describe(people: readonly MentionRecipient[], canAdd: boolean) {
  const names = people.map((person) => person.name).join(", ");
  const verb = people.length === 1 ? "is" : "are";
  return `${names} ${verb} not in this channel. ${
    canAdd
      ? "Invite them to the channel, or send without inviting them."
      : "You cannot add people to this channel. You can still send without inviting them."
  }`;
}

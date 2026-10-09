// Janet's lanes for attention turns: an Interest's turns run one at a time, in
// order, and only a few Interests run at once, so a burst of watch matches
// cannot take every process the agent has. A slot is freed after each turn and
// goes to the Interest that has waited longest, so a busy Interest cannot hold
// one while others wait.

/** Interests whose turns run at once. */
export const LANES = 2;
/** Turns one Interest can have waiting, and all Interests together, as in
 * Janet; later ones are dropped and logged. */
export const LANE_LIMIT = 8;
export const TOTAL_LIMIT = 32;

export class Lanes {
  private readonly queues = new Map<string, (() => Promise<unknown>)[]>();
  /** Lanes with work waiting for a free slot, longest waiting first. */
  private readonly ready: string[] = [];
  private readonly running = new Set<string>();
  private waiting = 0;
  private stopped = false;

  constructor(private readonly width = LANES) {}

  /** Queues `task` on `lane`, or says why it was not. */
  add(
    lane: string,
    task: () => Promise<unknown>,
  ): "queued" | "full" | "stopped" {
    if (this.stopped) return "stopped";
    const queue = this.queues.get(lane) ?? [];
    if (queue.length >= LANE_LIMIT || this.waiting >= TOTAL_LIMIT)
      return "full";
    queue.push(task);
    this.waiting++;
    this.queues.set(lane, queue);
    if (!this.running.has(lane) && !this.ready.includes(lane))
      this.ready.push(lane);
    this.pump();
    return "queued";
  }

  /** Drops every waiting turn; running ones finish. */
  clear() {
    this.queues.clear();
    this.ready.length = 0;
    this.waiting = 0;
  }
  /** Drops every waiting turn, and takes no more. */
  stop() {
    this.stopped = true;
    this.clear();
  }

  private pump() {
    while (this.running.size < this.width && this.ready.length) {
      const lane = this.ready.shift() as string;
      const task = this.queues.get(lane)?.shift();
      if (!task) {
        this.queues.delete(lane);
        continue;
      }
      this.waiting--;
      this.running.add(lane);
      void this.turn(lane, task);
    }
  }

  private async turn(lane: string, task: () => Promise<unknown>) {
    try {
      await task();
    } catch (error) {
      console.warn("An attention turn failed", error);
    }
    this.running.delete(lane);
    if (this.queues.get(lane)?.length) this.ready.push(lane);
    else this.queues.delete(lane);
    this.pump();
  }
}

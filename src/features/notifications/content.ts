import { workflowLabel } from "../relay/workflow-attribution";
import type { ChannelSummary, Profile } from "../relay/contracts";
import type { IncomingMessage } from "../relay/incoming";
import { scanMarkdown } from "../relay/message-content";
import type { NotificationCategory } from "./preferences";

export type NotificationText = Readonly<{ title: string; body: string }>;

// An isolated surrogate is not valid Unicode; native JSON rejects it.
const loneSurrogate =
  /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g;

function shortText(value: string, limit: number) {
  const points = Array.from(
    value
      .replace(loneSurrogate, "\uFFFD")
      .replace(/[\p{Cc}\p{Bidi_Control}]/gu, " ")
      .replace(/\s+/gu, " ")
      .trim(),
  );
  return points.length > limit
    ? `${points.slice(0, limit - 1).join("")}…`
    : points.join("");
}

type TextNode = { type: string; value?: string; children?: TextNode[] };
function prose(node: TextNode): string {
  if (node.type === "definition") return "";
  if (node.type === "image" || node.type === "imageReference") return "[Image]";
  if (node.type === "break" || node.type === "thematicBreak") return " ";
  if (node.value !== undefined) return node.value;
  const separator = [
    "paragraph",
    "heading",
    "emphasis",
    "strong",
    "link",
    "linkReference",
  ].includes(node.type)
    ? ""
    : " ";
  return (node.children ?? []).map(prose).join(separator);
}

/** Flattened, bounded prose; empty when nothing displayable remains. */
export function plainText(content: string) {
  // Cut the source on a whole code point, never between a surrogate pair.
  const end = /[\uD800-\uDBFF]/.test(content.charAt(4095)) ? 4095 : 4096;
  const { tree, tooDeep } = scanMarkdown(content.slice(0, end));
  return tooDeep ? "" : shortText(prose(tree), 200);
}

/** Bounded CommonMark text, not rendered HTML or a fetch of linked/attached content. */
export function messagePreview(content: string): string {
  return plainText(content) || "New message";
}

/** Plugin text gets the same flattening and bounds; missing text keeps the category fallback. */
export function pluginNotificationText(
  input: Readonly<{ title?: string; body?: string }>,
  label: string,
): NotificationText {
  return Object.freeze({
    title: shortText(input.title ?? "", 128) || "Buzz",
    body: plainText(input.body ?? "") || `New ${label.toLowerCase()}`,
  });
}

/** Names are optional cached enrichment; notifications never wait for profile reads. */
export function messageNotificationText(
  message: IncomingMessage,
  category: NotificationCategory,
  channel: ChannelSummary | undefined,
  profile: Profile | undefined,
  resolvedName?: string | undefined,
): NotificationText {
  const name =
    shortText(resolvedName ?? profile?.name ?? "", 64) ||
    (message.workflowOwnerId ?? message.authorId).slice(0, 10);
  const sender = message.workflowOwnerId ? workflowLabel(name) : name;
  const destination =
    channel?.channelType === "dm"
      ? "a direct message"
      : `#${shortText(channel?.name ?? "", 64) || message.channelId.slice(0, 8)}`;
  const title =
    category === "mention"
      ? `${sender} mentioned you in ${destination}`
      : category === "direct"
        ? `${sender} sent you a direct message`
        : `${sender} replied in ${destination}`;
  return Object.freeze({ title, body: messagePreview(message.previewContent) });
}

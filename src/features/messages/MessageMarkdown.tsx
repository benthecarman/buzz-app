import { useChannelIdentityNames } from "../identity-names/react";
import {
  Children,
  createContext,
  isValidElement,
  memo,
  useContext,
  useMemo,
  useState,
  type ComponentPropsWithoutRef,
  type ReactNode,
} from "react";
import type { RelaySession } from "../relay/session";
import { MessageLink } from "../conversation/MessageLink";
import { parseBuzzLink } from "../navigation/buzz-links";
import { messageLinkParts } from "./message-link-parts";
import {
  ReferenceText,
  channelForLink,
  channelLinkLabel,
  emptyReferenceDirectory,
} from "./ReferenceText";
import { AtIcon, RobotIcon } from "../../shared/design-system/icons/index";
import { profileKey } from "../profiles/target";
import referenceStyles from "../../shared/InlineReference.module.css";
import Markdown, {
  type Components,
  type ExtraProps,
  type UrlTransform,
} from "react-markdown";
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";
import { remarkSpoilers } from "./remark-spoilers";
import type { ConversationExtensions } from "../conversation/contracts";
import { InlineText } from "../conversation/InlineText";
import type { ChannelMessage, Profile } from "../relay/contracts";
import { emojiMatches, linkPart, messageParts } from "../relay/emoji";
import { safeLinkUrl } from "../relay/message-content";
import styles from "./Messages.module.css";
import { profileMentionParts } from "./profile-mentions";
import {
  isLiteralMarkdownContext,
  prepareMarkdown,
  type LiteralRange,
} from "./markdown-preparation";

type MarkdownNode = {
  type: string;
  value?: string;
  url?: string;
  title?: string;
  alt?: string;
  identifier?: string;
  label?: string;
  children?: MarkdownNode[];
  position?: { start: { offset?: number }; end: { offset?: number } };
  data?: {
    hName?: string;
    hProperties?: Record<string, string>;
  };
};

type InlinePart = {
  text: string;
  target?: string | undefined;
  literal?: boolean;
};
type ProtectedContent = {
  content: string;
  prefix: string;
  parts: InlinePart[];
};
/** Bind exact names on the FULL signed body, before Markdown decodes escapes or
 * divides emphasis. Reference labels must also survive unchanged for resolution. */
function protectInlineContent(
  row: Pick<
    ChannelMessage,
    | "content"
    | "edited"
    | "attachmentSeams"
    | "mentions"
    | "mentionReferences"
    | "emoji"
  >,
  profiles: ReadonlyMap<string, Profile> | undefined,
  literalRanges: readonly LiteralRange[],
  agents: typeof emptyReferenceDirectory.agents,
  spoilerDelimiters: readonly number[],
): ProtectedContent {
  let rangeIndex = 0;
  const isLiteral = (start: number, end: number) => {
    while (
      literalRanges[rangeIndex] &&
      (literalRanges[rangeIndex]?.end ?? 0) <= start
    )
      rangeIndex++;
    return (literalRanges[rangeIndex]?.start ?? Infinity) < end;
  };

  // Numeric entities can manufacture private-use characters during parsing too.
  // Choose an unused prefix in that decoded source; never trust a fixed marker.
  const decoded = row.content.replace(
    /&#(x[a-f\d]{1,6}|\d{1,7});/gi,
    (entity, number: string) => {
      const code =
        number[0]?.toLowerCase() === "x"
          ? Number.parseInt(number.slice(1), 16)
          : Number.parseInt(number, 10);
      return code <= 0x10ffff ? String.fromCodePoint(code) : entity;
    },
  );
  const used = new Set(
    [...decoded.matchAll(/[\uE000\uFFFC](\d+)\uE001/g)].map(
      (match) => match[1],
    ),
  );
  let nonce = 0;
  while (used.has(String(nonce))) nonce++;
  const prefix = `\uE000${nonce}\uE001`;
  const parts: InlinePart[] = [];
  const token = (part: InlinePart) => {
    parts.push(part);
    // Mention edges must keep their punctuation class for emphasis flanking:
    // @Honey* beside italic ! is punctuation on both sides, not a word. The
    // wire itself holds no emphasis there for a parser that does not split
    // out the mention first, since the name's star and the delimiter merge
    // into one run; the exact signed name was kept over italics that other
    // clients could read.
    // U+FFFC is a Unicode symbol, not Markdown syntax. Keep it inside the
    // nonce-protected token so restoration removes only generated characters.
    const start = part.target ? `\uFFFC${nonce}\uE001` : prefix;
    const end =
      part.target && /[\p{P}\p{S}]$/u.test(part.text) ? "\uFFFC" : "\uE002";
    return `${start}${parts.length - 1}${end}`;
  };
  let offset = 0;
  const content = profileMentionParts(row, profiles, agents)
    .map((segment) => {
      const start = offset;
      offset += segment.text.length;
      if (segment.target && !isLiteral(start, offset)) return token(segment);
      let partOffset = start;
      return messageParts(segment.text)
        .map((part) => {
          const partStart = partOffset;
          partOffset += part.length;
          const urlPart = linkPart(part);
          let result = "";
          let end = 0;
          // Protect explicitly encoded URL punctuation before GFM's fallback
          // autolinker decodes it. Restore as literal text, not another URL scan.
          const encoded = [...part.matchAll(/&#(58|46|64|x3a|x2e|x40);/gi)].map(
            (match) => ({
              start: match.index,
              end: match.index + match[0].length,
              literal: String.fromCodePoint(
                match[1]?.toLowerCase().startsWith("x")
                  ? Number.parseInt(match[1].slice(1), 16)
                  : Number(match[1]),
              ),
            }),
          );
          const replacements = [
            ...(urlPart
              ? []
              : Array.from(emojiMatches(part, row.emoji ?? []), (match) => ({
                  ...match,
                  literal: undefined,
                }))),
            ...(urlPart ? [] : encoded),
            ...spoilerDelimiters
              .filter(
                (start) =>
                  start >= partStart && start + 2 <= partStart + part.length,
              )
              .map((start) => ({
                start: start - partStart,
                end: start - partStart + 2,
                literal: undefined,
                spoiler: true,
              })),
          ].sort((a, b) => a.start - b.start);
          for (const match of replacements) {
            if (
              match.start < end ||
              isLiteral(partStart + match.start, partStart + match.end)
            )
              continue;
            result += part.slice(end, match.start);
            // Angle punctuation preserves the pipes' emphasis flanking while
            // `<` terminates GFM autolinks. The private-use first character is
            // not an HTML/autolink opener, so this remains a text delimiter.
            result +=
              "spoiler" in match
                ? `<${prefix}spoiler\uE002>`
                : token(
                    match.literal
                      ? { text: match.literal, literal: true }
                      : { text: part.slice(match.start, match.end) },
                  );
            end = match.end;
          }
          return result + part.slice(end);
        })
        .join("");
    })
    .join("");
  return { content, prefix, parts };
}

const placeholderPattern = (protectedContent: ProtectedContent) =>
  new RegExp(
    `[\uE000\uFFFC]${protectedContent.prefix.slice(1)}(\\d+)[\uE002\uFFFC]`,
    "g",
  );

function inlineProtectionKey(
  row: ChannelMessage,
  profiles: ReadonlyMap<string, Profile> | undefined,
  agents: typeof emptyReferenceDirectory.agents,
) {
  const identities = [...row.mentions, ...(row.mentionReferences ?? [])];
  const mentions = identities.map((id) => [id, profiles?.get(id)?.name]);
  const mentioned = new Set(identities);
  const agentNames = agents
    .filter((agent) => mentioned.has(agent.pubkey))
    .map((agent) => [agent.pubkey, agent.name]);
  const emoji = row.emoji?.map(({ shortcode, url }) => [shortcode, url]);
  return JSON.stringify([
    row.edited === true,
    row.attachmentSeams,
    mentions,
    agentNames,
    emoji,
  ]);
}

const phrasingParents = new Set([
  "paragraph",
  "heading",
  "emphasis",
  "strong",
  "delete",
  "link",
  "linkReference",
  "tableCell",
]);

/** Chat never renders raw HTML, but `<harness>` is still something the author
 * typed. Show the exact source as text, with chat line breaks, instead of
 * dropping it. Run last: the source is literal, so no mention, emoji or
 * autolink processing applies. */
function remarkLiteralHtml() {
  const literal = (value: string): MarkdownNode[] =>
    value
      .split(/\r?\n|\r/)
      .flatMap((line, index) => [
        ...(index ? [{ type: "break" }] : []),
        ...(line ? [{ type: "text", value: line }] : []),
      ]);
  return (tree: MarkdownNode) => {
    const visit = (parent: MarkdownNode) => {
      if (!parent.children) return;
      parent.children = parent.children.flatMap((child) => {
        if (child.type !== "html") {
          visit(child);
          return [child];
        }
        const text = literal(child.value ?? "");
        return phrasingParents.has(parent.type)
          ? text
          : [
              {
                type: "paragraph",
                children: text,
                ...(child.position ? { position: child.position } : {}),
              },
            ];
      });
    };
    visit(tree);
  };
}

/** Offer only Markdown prose to profile controls and inline plugins. */
function remarkInlineContent(protectedContent: ProtectedContent) {
  const restore = (value: string) =>
    value.replace(
      placeholderPattern(protectedContent),
      (match, index: string) =>
        protectedContent.parts[Number(index)]?.text ?? match,
    );
  const restoreLiteral = (node: MarkdownNode) => {
    for (const key of [
      "value",
      "url",
      "title",
      "alt",
      "identifier",
      "label",
    ] as const) {
      const value = node[key];
      if (typeof value === "string") node[key] = restore(value);
    }
    for (const child of node.children ?? []) restoreLiteral(child);
  };
  return (tree: MarkdownNode) => {
    const visit = (parent: MarkdownNode) => {
      if (isLiteralMarkdownContext(parent.type)) {
        restoreLiteral(parent);
        return;
      }
      if (!parent.children) return;
      parent.children = parent.children.flatMap((child) => {
        if (child.type !== "text" || typeof child.value !== "string") {
          visit(child);
          return [child];
        }
        const parts: InlinePart[] = [];
        let plain = "";
        let end = 0;
        for (const match of child.value.matchAll(
          placeholderPattern(protectedContent),
        )) {
          plain += child.value.slice(end, match.index);
          const part = protectedContent.parts[Number(match[1])];
          if (part?.target || part?.literal) {
            if (plain) parts.push({ text: plain });
            parts.push(part);
            plain = "";
          } else {
            plain += part?.text ?? match[0];
          }
          end = match.index + match[0].length;
        }
        plain += child.value.slice(end);
        if (plain) parts.push({ text: plain });
        return parts.map((part) => ({
          type: "buzzInlineContent",
          data: {
            hName: "span",
            hProperties: {
              "data-inline-text": part.text,
              ...(part.literal ? { "data-literal-text": "true" } : {}),
              ...(part.target ? { "data-profile-target": part.target } : {}),
            },
          },
        }));
      });
    };
    visit(tree);
  };
}

const transformUrl: UrlTransform = (value) =>
  parseBuzzLink(value) || profileKey(value) ? value : safeLinkUrl(value);
const labelText = (children: ReactNode): string =>
  Children.toArray(children)
    .map((child) =>
      isValidElement<{ children?: ReactNode }>(child)
        ? labelText(child.props.children)
        : String(child),
    )
    .join("");

type MessageComponents = {
  [Tag in "p" | "a" | "img" | "span"]: (
    props: ComponentPropsWithoutRef<Tag> & ExtraProps,
  ) => ReactNode;
};
const MessageComponentsContext = createContext<MessageComponents | null>(null);
function useMessageComponents() {
  const components = useContext(MessageComponentsContext);
  if (!components) throw new Error("Missing message rendering context");
  return components;
}
// Keep component types stable: profile/directory updates must not remount a
// focused link or discard its pending preview timer.
const markdownComponents: Components = {
  p: (props) => useMessageComponents().p(props),
  a: (props) => useMessageComponents().a(props),
  img: (props) => useMessageComponents().img(props),
  span: (props) => useMessageComponents().span(props),
};

export function MessageMarkdown({
  row,
  directory = emptyReferenceDirectory,
  session,
  scope,
  extensions,
  media,
  onOpenLink,
  canOpenLink,
  participantProfiles,
  largeEmoji = false,
  interactive = true,
}: {
  row: ChannelMessage;
  directory?: typeof emptyReferenceDirectory;
  session?: RelaySession | undefined;
  scope?: string | undefined;
  extensions?: ConversationExtensions | undefined;
  media(url: string): string | undefined;
  onOpenLink(url: string): boolean;
  canOpenLink?: ((target: string) => boolean) | undefined;
  participantProfiles?: ReadonlyMap<string, Profile> | undefined;
  largeEmoji?: boolean | undefined;
  interactive?: boolean;
}) {
  const resolveName = useChannelIdentityNames(session, row.channelId);
  const prepared = useMemo(() => prepareMarkdown(row.content), [row.content]);
  if (prepared.kind === "plain")
    return <div className={styles.plainText}>{prepared.content}</div>;
  return (
    <PreparedMessageMarkdown
      row={row}
      prepared={prepared}
      directory={directory}
      session={session}
      scope={scope}
      extensions={extensions}
      media={media}
      onOpenLink={onOpenLink}
      canOpenLink={canOpenLink}
      participantProfiles={participantProfiles}
      resolveName={resolveName}
      largeEmoji={largeEmoji}
      interactive={interactive}
    />
  );
}

function PreparedMessageMarkdown({
  row: sourceRow,
  prepared,
  directory = emptyReferenceDirectory,
  session,
  scope,
  extensions,
  media,
  onOpenLink,
  canOpenLink,
  participantProfiles,
  resolveName,
  largeEmoji = false,
  interactive = true,
}: Parameters<typeof MessageMarkdown>[0] & {
  prepared: Extract<ReturnType<typeof prepareMarkdown>, { kind: "markdown" }>;
  resolveName: (pubkey: string, fallback: string) => string;
}) {
  const row =
    prepared.content === sourceRow.content
      ? sourceRow
      : { ...sourceRow, content: prepared.content };
  const renderLink = (url: string, label?: string, children?: ReactNode) => {
    const channel = channelForLink(url, directory.channels);
    return (
      <MessageLink
        url={url}
        label={label}
        directoryLabel={channelLinkLabel(url, directory.channels)}
        registry={extensions?.links}
        onOpenLink={onOpenLink}
        session={session}
        scope={scope}
        interactive={interactive}
        channelPrivate={!!channel?.private}
      >
        {children}
      </MessageLink>
    );
  };
  const renderInline = (text: string) =>
    extensions ? (
      <InlineText
        registry={extensions.inline}
        content={{ text, message: row }}
        media={media}
      />
    ) : (
      text
    );
  const renderText = (text: string) => {
    let offset = 0;
    return messageLinkParts(text).map((part) => {
      const key = `${offset}:${part.text}`;
      offset += part.text.length;
      return part.url ? (
        <span key={key}>{renderLink(part.url, part.label)}</span>
      ) : (
        <ReferenceText
          channelId={row.channelId}
          key={key}
          text={part.text}
          mentions={[]}
          directory={directory}
          renderText={renderInline}
          onOpenLink={onOpenLink}
          extensions={extensions}
          session={session}
          scope={scope}
          interactive={interactive}
        />
      );
    });
  };

  const profiles = participantProfiles ?? directory.profiles;
  const protectionKey = inlineProtectionKey(
    sourceRow,
    profiles,
    directory.agents,
  );
  // The key contains every row/profile/agent/emoji value consumed below. It
  // avoids reparsing for equivalent folded rows without hiding live inputs.
  // biome-ignore lint/correctness/useExhaustiveDependencies: semantic key
  const protectedContent = useMemo(
    () =>
      protectInlineContent(
        {
          content: prepared.content,
          // Seams index the folded body. Leave link-repaired prose unbound, as edits are.
          ...(sourceRow.edited ||
          (sourceRow.attachmentSeams && prepared.content !== sourceRow.content)
            ? { edited: true as const }
            : {}),
          ...(sourceRow.attachmentSeams
            ? { attachmentSeams: sourceRow.attachmentSeams }
            : {}),
          mentions: sourceRow.mentions,
          mentionReferences: sourceRow.mentionReferences ?? [],
          ...(sourceRow.emoji ? { emoji: sourceRow.emoji } : {}),
        },
        profiles,
        prepared.literalRanges,
        directory.agents,
        prepared.spoilerDelimiters,
      ),
    [
      prepared.content,
      prepared.literalRanges,
      prepared.spoilerDelimiters,
      protectionKey,
    ],
  );
  // Explicit profile links are identity locators, not signed notification intent.
  // Reuse the same control and availability gate as bound prose mentions.
  const renderProfile = (text: unknown, target: unknown) => {
    const key = typeof target === "string" ? profileKey(target) : undefined;
    const agent =
      !!key &&
      (directory.agents.some((agent) => agent.pubkey === key) ||
        (participantProfiles ?? directory.profiles).get(key)?.isAgent);
    const clickable =
      interactive && typeof target === "string" && !!canOpenLink?.(target);
    if (
      typeof text === "string" &&
      typeof target === "string" &&
      (!interactive || clickable || agent)
    ) {
      const label = key ? resolveName(key, text.slice(1)) : text.slice(1);
      const Icon = agent ? RobotIcon : AtIcon;
      const Mention = clickable ? "button" : "span";
      // Selection copy reads the identity and the whole label (selection-copy.ts).
      return (
        <Mention
          type={clickable ? "button" : undefined}
          className={referenceStyles.link}
          data-mention-kind={agent ? "agent" : "person"}
          data-profile-target={target}
          data-mention-name={label}
          aria-label={clickable ? `View ${label} profile` : undefined}
          onClick={
            clickable
              ? (event) => {
                  event.currentTarget.focus();
                  onOpenLink(target);
                }
              : undefined
          }
        >
          <Icon aria-hidden="true" className={referenceStyles.icon} />
          {label}
        </Mention>
      );
    }
    return undefined;
  };
  const components: MessageComponents = {
    p: ({ node: _node, ...props }) => (
      <p
        {...props}
        className={largeEmoji ? styles.text : undefined}
        data-single-emoji={largeEmoji || undefined}
      />
    ),
    a: ({ href, children }) => {
      if (href && profileKey(href)) {
        const label = labelText(children);
        if (!interactive || !canOpenLink?.(href))
          return (
            <span>
              {children} ({href})
            </span>
          );
        return renderProfile(label.startsWith("@") ? label : `@${label}`, href);
      }
      return href ? (
        renderLink(
          href,
          labelText(children) === href ? undefined : labelText(children),
          labelText(children) === href ? undefined : children,
        )
      ) : (
        <span>{children}</span>
      );
    },
    img: ({ node: _node, alt }) =>
      alt ? <span className={styles.imageAlt}>{alt}</span> : null,
    span: ({ node: _node, children, ...props }) => {
      if ((props as Record<string, unknown>)["data-spoiler"] === "true")
        return (
          <MessageSpoiler key={row.content} interactive={interactive}>
            {children}
          </MessageSpoiler>
        );
      const {
        "data-inline-text": text,
        "data-profile-target": target,
        "data-literal-text": literalText,
      } = props as typeof props & {
        "data-inline-text"?: unknown;
        "data-profile-target"?: unknown;
        "data-literal-text"?: unknown;
      };
      const profile = renderProfile(text, target);
      if (profile) return profile;
      return typeof text === "string" ? (
        literalText === "true" ? (
          text
        ) : (
          renderText(text)
        )
      ) : (
        <span {...props}>{children}</span>
      );
    },
  };

  const markdown = (
    <MessageComponentsContext value={components}>
      <MarkdownBody protectedContent={protectedContent} />
    </MessageComponentsContext>
  );
  return largeEmoji ? markdown : <div className={styles.text}>{markdown}</div>;
}

const MarkdownBody = memo(function MarkdownBody({
  protectedContent,
}: {
  protectedContent: ProtectedContent;
}) {
  return (
    <Markdown
      remarkPlugins={[
        remarkGfm,
        remarkBreaks,
        [remarkSpoilers, `<${protectedContent.prefix}spoiler\uE002>`],
        [remarkInlineContent, protectedContent],
        remarkLiteralHtml,
      ]}
      components={markdownComponents}
      skipHtml
      urlTransform={transformUrl}
    >
      {protectedContent.content}
    </Markdown>
  );
});

function MessageSpoiler({
  children,
  interactive,
}: {
  children: ReactNode;
  interactive: boolean;
}) {
  const [revealed, setRevealed] = useState(false);
  return (
    <span className={styles.spoiler} data-revealed={revealed}>
      {interactive ? (
        <button
          type="button"
          className={styles.spoilerMask}
          aria-label={revealed ? "Hide spoiler" : "Reveal spoiler"}
          aria-expanded={revealed}
          onClick={(event) => {
            event.stopPropagation();
            setRevealed(!revealed);
          }}
        />
      ) : (
        <span className={styles.spoilerMask} aria-hidden="true" />
      )}
      <span
        className={styles.spoilerContent}
        aria-hidden={!revealed}
        inert={!revealed}
      >
        {children}
      </span>
    </span>
  );
}

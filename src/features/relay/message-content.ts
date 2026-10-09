import { fromMarkdown } from "mdast-util-from-markdown";

export const MAX_MARKDOWN_LENGTH = 100_000;
export const MAX_MARKDOWN_DEPTH = 100;

/** Active links accept credential-free HTTP(S) destinations. */
export function safeLinkUrl(value: string): string | undefined {
  try {
    const url = new URL(value);
    return (url.protocol === "https:" || url.protocol === "http:") &&
      !url.username &&
      !url.password
      ? url.href
      : undefined;
  } catch {
    return undefined;
  }
}

/** Projected attachments and imeta media stay HTTPS-only. */
export function safeMessageUrl(value: string): string | undefined {
  const url = safeLinkUrl(value);
  return url?.startsWith("https:") ? url : undefined;
}

type MarkdownNode = {
  type: string;
  value?: string;
  url?: string;
  identifier?: string;
  children?: MarkdownNode[];
  position?: { start: { offset?: number }; end: { offset?: number } };
};

type MarkdownScan = {
  tree: MarkdownNode;
  tooDeep: boolean;
  definitions: Map<string, string>;
  images: MarkdownNode[];
  links: MarkdownNode[];
};

/** Raw HTML in chat is shown as the typed text, except complete HTML
 * comments. Comments are hidden markup by Markdown convention, and clients
 * put protocol markers in them (Buzz Desktop waves start with
 * `<!-- buzz:wave:v1 -->`). An unclosed `<!--` stays visible. */
export function visibleHtmlSource(value: string): string {
  return value.replace(/<!--[\s\S]*?-->/g, "");
}

export const MAX_ATTACHMENT_NAME_LENGTH = 256;
export const RELAY_HASH_BASENAME = /^[0-9a-f]{64}(?:\.[^./?#]+)?$/i;

export function relayHashBasename(value: string): boolean {
  return RELAY_HASH_BASENAME.test(value);
}

export function safeAttachmentName(value: string): string | undefined {
  const name = value.slice(0, MAX_ATTACHMENT_NAME_LENGTH);
  return name && !hasUnsafeAttachmentNameCharacter(name) ? name : undefined;
}

function hasUnsafeAttachmentNameCharacter(value: string): boolean {
  for (const character of value) {
    const code = character.codePointAt(0);
    if (code === undefined) continue;
    if (code <= 0x1f || (code >= 0x7f && code <= 0x9f)) return true;
    if (code === 0x200e || code === 0x200f) return true;
    if (code >= 0x202a && code <= 0x202e) return true;
    if (code >= 0x2066 && code <= 0x2069) return true;
  }
  return false;
}

/** Parse once and bound attacker-controlled nesting before recursive render stages. */
export function scanMarkdown(content: string): MarkdownScan {
  const tree = fromMarkdown(content) as MarkdownNode;
  const definitions = new Map<string, string>();
  const images: MarkdownNode[] = [];
  const links: MarkdownNode[] = [];
  const pending = [{ node: tree, depth: 0 }];
  let tooDeep = false;
  while (pending.length) {
    const current = pending.pop();
    if (!current) continue;
    const { node, depth } = current;
    if (depth > MAX_MARKDOWN_DEPTH) {
      tooDeep = true;
      continue;
    }
    if (
      node.type === "definition" &&
      typeof node.identifier === "string" &&
      typeof node.url === "string" &&
      !definitions.has(node.identifier.toLowerCase())
    )
      definitions.set(node.identifier.toLowerCase(), node.url);
    if (node.type === "image" || node.type === "imageReference")
      images.push(node);
    if (node.type === "link" || node.type === "linkReference") links.push(node);
    for (let index = (node.children?.length ?? 0) - 1; index >= 0; index--) {
      const child = node.children?.[index];
      if (child) pending.push({ node: child, depth: depth + 1 });
    }
  }
  return { tree, tooDeep, definitions, images, links };
}

function resolvedNodeUrl(
  node: MarkdownNode,
  definitions: ReadonlyMap<string, string>,
): string | undefined {
  return node.type === "link" || node.type === "image"
    ? node.url
    : typeof node.identifier === "string"
      ? definitions.get(node.identifier.toLowerCase())
      : undefined;
}

/** Seams are the stripped body's offsets where removed text once separated its
 * neighbors; only interior ones can join text that was not adjacent. */
function stripRanges(
  content: string,
  ranges: readonly { start: number; end: number }[],
): { content: string; seams: readonly number[] } {
  const merged: Array<{ start: number; end: number }> = [];
  for (const range of [...ranges].sort((a, b) => a.start - b.start)) {
    const previous = merged.at(-1);
    if (previous && range.start <= previous.end) {
      previous.end = Math.max(previous.end, range.end);
      continue;
    }
    merged.push({ ...range });
  }

  let stripped = "";
  let offset = 0;
  const seams: number[] = [];
  for (const range of merged) {
    stripped += content.slice(offset, range.start);
    seams.push(stripped.length);
    offset = range.end;
  }
  stripped = (stripped + content.slice(offset)).trimEnd();
  return {
    content: stripped,
    seams: Object.freeze(
      seams.filter((seam) => seam > 0 && seam < stripped.length),
    ),
  };
}

function nodeText(node: MarkdownNode): string {
  if (typeof node.value === "string") return node.value;
  return (node.children ?? []).map(nodeText).join("");
}

function adoptableAttachmentLabel(label: string, url: string): boolean {
  if (relayHashBasename(label)) return false;
  try {
    new URL(label);
    return false;
  } catch {
    return label !== url;
  }
}

export type ProjectedAttachmentLinkName = Readonly<{
  url: string;
  name: string;
}>;

export function projectMarkdownAttachments(
  content: string,
  attachmentUrls: ReadonlySet<string>,
): {
  content: string;
  seams: readonly number[];
  urls: readonly string[];
  names: readonly ProjectedAttachmentLinkName[];
} {
  const { tooDeep, definitions, images, links } = scanMarkdown(content);
  if (tooDeep)
    return {
      content,
      seams: Object.freeze([]),
      urls: Object.freeze([]),
      names: Object.freeze([]),
    };

  const urls: string[] = [];
  const seen = new Set<string>();
  const names: ProjectedAttachmentLinkName[] = [];
  const ranges: Array<{ start: number; end: number }> = [];

  for (const image of images) {
    const url = resolvedNodeUrl(image, definitions);
    const safeUrl = url ? safeMessageUrl(url) : undefined;
    if (safeUrl && !seen.has(safeUrl)) {
      seen.add(safeUrl);
      urls.push(safeUrl);
    }
    const start = image.position?.start.offset;
    const end = image.position?.end.offset;
    if (typeof start === "number" && typeof end === "number")
      ranges.push({ start, end });
  }

  if (attachmentUrls.size) {
    for (const link of links) {
      const raw = resolvedNodeUrl(link, definitions);
      const url = raw ? safeMessageUrl(raw) : undefined;
      if (!url || !attachmentUrls.has(url)) continue;

      const start = link.position?.start.offset;
      const end = link.position?.end.offset;
      if (typeof start === "number" && typeof end === "number")
        ranges.push({ start, end });

      const label = safeAttachmentName(nodeText(link).trim());
      if (!label || !adoptableAttachmentLabel(label, url)) continue;
      names.push({ url, name: label });
    }
  }

  return {
    ...stripRanges(content, ranges),
    urls: Object.freeze(urls),
    names: Object.freeze(names),
  };
}

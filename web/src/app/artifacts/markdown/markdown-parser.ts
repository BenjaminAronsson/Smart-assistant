/**
 * A markdown **parser only** — no rendering, no HTML strings, no `innerHTML`.
 * This is the security boundary for the Markdown/HTML artifact renderer
 * (docs/02 §6, F3b.3 threat note): artifact bytes are untrusted (they can come
 * from a fetched web page or a coding-worker output), so this module turns raw
 * text into a small, closed set of block/inline node types that the renderer
 * component builds DOM from via Angular's auto-escaping text bindings —
 * `<script>`, an `onerror=` attribute, or a `javascript:` link in the source
 * text can only ever end up as literal characters in a text node, never as a
 * tag, attribute, or navigation.
 *
 * Deliberately **the safe subset**, not a full CommonMark implementation:
 * headings, paragraphs, lists (ordered/unordered, one level), fenced code
 * blocks, links, and emphasis/strong/inline-code. No raw HTML passthrough (by
 * design — HTML tags in the source are inert text, see above), no images, no
 * tables, no nested inline formatting. This is the simpler operational choice
 * over pulling in a markdown dependency, per the feature's own instructions.
 */

export type MdInline =
  | { type: 'text'; value: string }
  | { type: 'strong'; value: string }
  | { type: 'em'; value: string }
  | { type: 'code'; value: string }
  /** A `link` node is only ever produced once its href has passed
   * {@link sanitizeHref} — when it fails, `parseInline` downgrades the whole
   * node to plain `text` instead, so a renderer never has to re-check. */
  | { type: 'link'; href: string; value: string };

export type MdBlock =
  | { type: 'heading'; level: 1 | 2 | 3 | 4 | 5 | 6; inline: MdInline[] }
  | { type: 'paragraph'; inline: MdInline[] }
  | { type: 'list'; ordered: boolean; items: MdInline[][] }
  | { type: 'code'; code: string; lang: string | null };

import { sanitizeHref } from '../safe-url';

const HEADING_RE = /^(#{1,6})\s+(.*)$/;
const UL_RE = /^\s*[-*+]\s+(.*)$/;
const OL_RE = /^\s*\d+\.\s+(.*)$/;
const FENCE_RE = /^```(.*)$/;
const CLOSING_FENCE_RE = /^```\s*$/;

/** Token scanner for one line's worth of inline content — code span first (it
 * must not have its own contents re-scanned for `*`/`_`/`[`), then
 * strong/em/link, longest delimiters first so `**x**` never parses as two
 * `*` runs. */
const INLINE_RE =
  /`([^`]+)`|\*\*([^*]+)\*\*|__([^_]+)__|\*([^*]+)\*|_([^_]+)_|\[([^\]]+)\]\(([^)\s]+)\)/g;

function parseInline(text: string): MdInline[] {
  const out: MdInline[] = [];
  let last = 0;
  for (const m of text.matchAll(INLINE_RE)) {
    const index = m.index ?? 0;
    if (index > last) {
      out.push({ type: 'text', value: text.slice(last, index) });
    }
    if (m[1] !== undefined) {
      out.push({ type: 'code', value: m[1] });
    } else if (m[2] !== undefined || m[3] !== undefined) {
      out.push({ type: 'strong', value: m[2] ?? m[3] });
    } else if (m[4] !== undefined || m[5] !== undefined) {
      out.push({ type: 'em', value: m[4] ?? m[5] });
    } else if (m[6] !== undefined && m[7] !== undefined) {
      const href = sanitizeHref(m[7]);
      out.push(href === null ? { type: 'text', value: m[6] } : { type: 'link', href, value: m[6] });
    }
    last = index + m[0].length;
  }
  if (last < text.length) {
    out.push({ type: 'text', value: text.slice(last) });
  }
  return out;
}

/** Sanitize a fence's language tag to a short token — it only ever becomes a
 * CSS class-ish label, never markup, but a defensive allow-list keeps it that
 * way even if a renderer changes later. */
function safeLang(raw: string): string | null {
  const trimmed = raw.trim();
  return trimmed.length > 0 && /^[A-Za-z0-9_+-]+$/.test(trimmed) ? trimmed : null;
}

export function parseMarkdown(source: string): MdBlock[] {
  const lines = source.split(/\r\n|\r|\n/);
  const blocks: MdBlock[] = [];
  let i = 0;

  while (i < lines.length) {
    const line = lines[i];

    if (line.trim().length === 0) {
      i++;
      continue;
    }

    const fence = FENCE_RE.exec(line);
    if (fence) {
      const lang = safeLang(fence[1]);
      const codeLines: string[] = [];
      i++;
      while (i < lines.length && !CLOSING_FENCE_RE.test(lines[i])) {
        codeLines.push(lines[i]);
        i++;
      }
      i++; // consume closing fence (or EOF — an unterminated fence still ends the block)
      blocks.push({ type: 'code', code: codeLines.join('\n'), lang });
      continue;
    }

    const heading = HEADING_RE.exec(line);
    if (heading) {
      const level = heading[1].length as 1 | 2 | 3 | 4 | 5 | 6;
      blocks.push({ type: 'heading', level, inline: parseInline(heading[2]) });
      i++;
      continue;
    }

    const ulMatch = UL_RE.exec(line);
    const olMatch = OL_RE.exec(line);
    if (ulMatch || olMatch) {
      const ordered = !!olMatch;
      const items: MdInline[][] = [];
      while (i < lines.length) {
        const m = ordered ? OL_RE.exec(lines[i]) : UL_RE.exec(lines[i]);
        if (!m) break;
        items.push(parseInline(m[1]));
        i++;
      }
      blocks.push({ type: 'list', ordered, items });
      continue;
    }

    // Paragraph: consecutive non-blank, non-special lines, joined with a space.
    const paraLines: string[] = [];
    while (
      i < lines.length &&
      lines[i].trim().length > 0 &&
      !HEADING_RE.test(lines[i]) &&
      !UL_RE.test(lines[i]) &&
      !OL_RE.test(lines[i]) &&
      !FENCE_RE.test(lines[i])
    ) {
      paraLines.push(lines[i].trim());
      i++;
    }
    blocks.push({ type: 'paragraph', inline: parseInline(paraLines.join(' ')) });
  }

  return blocks;
}

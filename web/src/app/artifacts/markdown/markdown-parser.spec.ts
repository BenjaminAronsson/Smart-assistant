import { parseMarkdown, type MdInline } from './markdown-parser';

function flatten(inline: MdInline[]): string {
  return inline.map((n) => n.value).join('');
}

describe('parseMarkdown', () => {
  it('parses a heading', () => {
    const blocks = parseMarkdown('## Title here');
    expect(blocks).toEqual([{ type: 'heading', level: 2, inline: [{ type: 'text', value: 'Title here' }] }]);
  });

  it('parses a paragraph with emphasis, strong and inline code', () => {
    const blocks = parseMarkdown('Some **bold** and *italic* and `code`.');
    expect(blocks.length).toBe(1);
    expect(blocks[0].type).toBe('paragraph');
    const inline = (blocks[0] as { inline: MdInline[] }).inline;
    expect(inline).toContain(jasmine.objectContaining({ type: 'strong', value: 'bold' }));
    expect(inline).toContain(jasmine.objectContaining({ type: 'em', value: 'italic' }));
    expect(inline).toContain(jasmine.objectContaining({ type: 'code', value: 'code' }));
  });

  it('parses an unordered and ordered list as separate blocks', () => {
    const blocks = parseMarkdown('- one\n- two\n\n1. first\n2. second');
    expect(blocks).toEqual([
      { type: 'list', ordered: false, items: [[{ type: 'text', value: 'one' }], [{ type: 'text', value: 'two' }]] },
      {
        type: 'list',
        ordered: true,
        items: [[{ type: 'text', value: 'first' }], [{ type: 'text', value: 'second' }]],
      },
    ]);
  });

  it('parses a fenced code block verbatim, including its language tag', () => {
    const blocks = parseMarkdown('```rust\nfn main() {}\n```');
    expect(blocks).toEqual([{ type: 'code', code: 'fn main() {}', lang: 'rust' }]);
  });

  it('parses a safe link', () => {
    const blocks = parseMarkdown('[docs](https://example.com/page)');
    const inline = (blocks[0] as { inline: MdInline[] }).inline;
    expect(inline).toEqual([{ type: 'link', href: 'https://example.com/page', value: 'docs' }]);
  });

  // --- Threat note: malicious artifact bytes must render as inert text. ---

  it('never produces a node type that could become a script element', () => {
    const blocks = parseMarkdown('<script>alert(document.cookie)</script>');
    expect(blocks[0].type).toBe('paragraph');
    const inline = (blocks[0] as { inline: MdInline[] }).inline;
    // The whole thing survives only as literal text nodes — no tag was parsed.
    expect(flatten(inline)).toContain('<script>alert(document.cookie)</script>');
    expect(inline.every((n) => n.type === 'text')).toBeTrue();
  });

  it('treats an <img onerror=...> payload as inert text, not a link or image node', () => {
    const blocks = parseMarkdown('<img src=x onerror=alert(1)>');
    const inline = (blocks[0] as { inline: MdInline[] }).inline;
    expect(inline.every((n) => n.type === 'text')).toBeTrue();
    expect(flatten(inline)).toContain('onerror=alert(1)');
  });

  it('downgrades a javascript: link to plain text instead of an unsafe href', () => {
    const blocks = parseMarkdown('[click me](javascript:alert(1))');
    const inline = (blocks[0] as { inline: MdInline[] }).inline;
    expect(inline.every((n) => n.type !== 'link')).toBeTrue();
    expect(flatten(inline)).toContain('click me');
  });

  it('downgrades a data: URI link to plain text', () => {
    const blocks = parseMarkdown('[open](data:text/html,<script>alert(1)</script>)');
    const inline = (blocks[0] as { inline: MdInline[] }).inline;
    expect(inline.every((n) => n.type !== 'link')).toBeTrue();
  });

  it('never interprets an onclick attribute as anything but text', () => {
    const blocks = parseMarkdown('<div onclick="alert(1)">text</div>');
    const inline = (blocks[0] as { inline: MdInline[] }).inline;
    expect(inline.every((n) => n.type === 'text')).toBeTrue();
    expect(flatten(inline)).toContain('onclick="alert(1)"');
  });

  it('sanitizes an unsafe fence language tag to null rather than passing it through', () => {
    const blocks = parseMarkdown('```"><script>alert(1)</script>\ncode\n```');
    expect(blocks[0]).toEqual(jasmine.objectContaining({ type: 'code', lang: null }));
  });
});

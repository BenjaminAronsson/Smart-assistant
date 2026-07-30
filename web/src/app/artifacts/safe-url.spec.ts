import { sanitizeHref } from './safe-url';

describe('sanitizeHref', () => {
  it('allows http, https and mailto', () => {
    expect(sanitizeHref('https://example.com/page')).toBe('https://example.com/page');
    expect(sanitizeHref('http://example.com')).toBe('http://example.com');
    expect(sanitizeHref('mailto:a@example.com')).toBe('mailto:a@example.com');
  });

  it('rejects javascript: navigation', () => {
    expect(sanitizeHref('javascript:alert(1)')).toBeNull();
  });

  it('rejects data: URIs', () => {
    expect(sanitizeHref('data:text/html,<script>alert(1)</script>')).toBeNull();
  });

  it('rejects a schemeless/relative reference', () => {
    expect(sanitizeHref('//evil.example.com')).toBeNull();
    expect(sanitizeHref('/relative/path')).toBeNull();
  });

  it('rejects malformed input', () => {
    expect(sanitizeHref('https://')).toBeNull();
    expect(sanitizeHref('')).toBeNull();
  });

  it('trims surrounding whitespace before checking the scheme', () => {
    expect(sanitizeHref('  https://example.com  ')).toBe('https://example.com');
  });
});

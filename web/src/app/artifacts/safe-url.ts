/**
 * Href sanitization shared by every renderer that turns artifact-supplied text
 * into a clickable link (docs/02 §6, F3b.3 threat note).
 *
 * Artifact bytes — and the `sources` provenance list that rides alongside them
 * — can originate from a fetched web page or a worker, so a link target is
 * untrusted input, not just untrusted markup. This is an **allow-list**, not a
 * deny-list: only `http:`, `https:`, and `mailto:` survive. Allow-listing
 * matters here because deny-listing individual dangerous schemes (blocking the
 * literal string `javascript:`) is exactly the pattern browsers' historic
 * `java\tscript:`-style whitespace tricks were built to defeat — an allow-list
 * of three schemes has no such bypass surface.
 */
const SAFE_SCHEME = /^(https?|mailto):/i;

/**
 * Returns the href if it is safe to bind into an anchor, or `null` if the
 * caller should render the text as inert text instead. Never throws.
 */
export function sanitizeHref(raw: string): string | null {
  const trimmed = raw.trim();
  if (!SAFE_SCHEME.test(trimmed)) {
    return null;
  }
  try {
    // Validates well-formedness in addition to the scheme prefix above; a
    // string that merely starts with "https:" but isn't a parseable URL is
    // not a safe navigation target either.
    new URL(trimmed);
  } catch {
    return null;
  }
  return trimmed;
}

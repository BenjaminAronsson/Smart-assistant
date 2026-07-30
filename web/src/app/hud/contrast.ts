/**
 * WCAG contrast maths for the glass-contrast audit (docs/12 §5/§9).
 *
 * The audit is not decorative: docs/12 §5's rule is "any text over an
 * unpredictable background must sit on glass or scrim — never raw", and §9 makes
 * "both wallpapers pass contrast audit" an acceptance gate. Doing it numerically
 * (composite the glass layer over the worst-case wallpaper pixel, then measure)
 * makes the gate a test rather than an eyeball judgement.
 */

export interface Rgb {
  r: number;
  g: number;
  b: number;
}

/** Parse `#rrggbb` (the form every token in `styles.scss` uses). */
export function parseHex(hex: string): Rgb {
  const value = hex.trim().replace('#', '');
  if (!/^[0-9a-fA-F]{6}$/.test(value)) {
    throw new Error(`expected #rrggbb, got "${hex}"`);
  }
  return {
    r: parseInt(value.slice(0, 2), 16),
    g: parseInt(value.slice(2, 4), 16),
    b: parseInt(value.slice(4, 6), 16),
  };
}

/**
 * Composite `fg` over `bg` at `alpha` (0-1) — "source over" in straight alpha.
 * This is what a `--glass-bg` panel does to the wallpaper behind it.
 */
export function composite(fg: Rgb, bg: Rgb, alpha: number): Rgb {
  const mix = (f: number, b: number) => Math.round(f * alpha + b * (1 - alpha));
  return { r: mix(fg.r, bg.r), g: mix(fg.g, bg.g), b: mix(fg.b, bg.b) };
}

/** WCAG 2.1 relative luminance. */
export function relativeLuminance({ r, g, b }: Rgb): number {
  const channel = (raw: number) => {
    const c = raw / 255;
    return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
}

/** WCAG 2.1 contrast ratio, always ≥ 1. */
export function contrastRatio(a: Rgb, b: Rgb): number {
  const la = relativeLuminance(a);
  const lb = relativeLuminance(b);
  const [hi, lo] = la >= lb ? [la, lb] : [lb, la];
  return (hi + 0.05) / (lo + 0.05);
}

/** WCAG AA thresholds (docs/12 §5): body text and large text. */
export const AA_BODY = 4.5;
export const AA_LARGE = 3;

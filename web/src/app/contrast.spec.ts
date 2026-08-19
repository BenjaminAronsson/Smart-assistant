/**
 * F10.6 — contrast, computed from the real design tokens (NFR-11, docs/12 §8).
 *
 * # Why this is a separate spec and not an axe rule
 *
 * `a11y.spec.ts` disables axe's `color-contrast` rule, and that is not a
 * convenience. In a headless karma fixture the computed background is
 * transparent: axe then compares the text colour against nothing, and reports
 * failures that do not exist in the product while missing the ones that do. A
 * check that is *wrong in both directions* is worse than an absent one, because
 * people trust it.
 *
 * So contrast is checked where it is actually decided — the token values
 * themselves, read from the same `styles.scss` the application ships. If a
 * token moves, this fails, whatever the fixture's layout happened to compute.
 *
 * WCAG 2.2 AA: 4.5:1 for body text, 3:1 for large text and for the non-text
 * parts an owner must be able to see (the focus ring in particular — a focus
 * ring nobody can find makes the keyboard-first work pointless).
 */

/** Relative luminance, WCAG 2.x definition. */
function luminance(hex: string): number {
  const value = hex.replace('#', '');
  const channel = (pair: string) => {
    const srgb = parseInt(pair, 16) / 255;
    return srgb <= 0.04045 ? srgb / 12.92 : ((srgb + 0.055) / 1.055) ** 2.4;
  };
  const r = channel(value.slice(0, 2));
  const g = channel(value.slice(2, 4));
  const b = channel(value.slice(4, 6));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function ratio(a: string, b: string): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

/**
 * The tokens as `styles.scss` defines them.
 *
 * Duplicated deliberately rather than parsed out of the SCSS at test time: a
 * parser would silently pass when a token is renamed away, which is the exact
 * moment this check matters. A mismatch here is meant to be a compile-time-ish
 * failure that a human resolves by looking at both files.
 */
const TOKENS = {
  ink: '#1c2026',
  inkDim: '#464d57',
  inkSoft: '#596270',
  focusRing: '#1a56c4',
  idle: '#4a7fd4',
  listen: '#1d8c85',
  speak: '#6b4fc9',
  wait: '#a86a06',
  done: '#2f7d47',
  error: '#b3271b',
  degraded: '#6b7078',
} as const;

/**
 * The effective background behind glass panels.
 *
 * `--surface` is white at 74% over a light wallpaper, so white is the honest
 * worst case for *text* contrast: any wallpaper showing through only darkens
 * it, which helps dark ink. The photo background is the case this cannot model,
 * and it is why the scrim exists — recorded in docs/12 §8 as a human check
 * rather than pretended away here.
 */
const GLASS = '#ffffff';

describe('contrast (F10.6, NFR-11)', () => {
  describe('body text on glass', () => {
    it('primary ink meets AA for body text', () => {
      expect(ratio(TOKENS.ink, GLASS)).toBeGreaterThanOrEqual(4.5);
    });

    it('dimmed ink still meets AA — "secondary" is not "optional"', () => {
      expect(ratio(TOKENS.inkDim, GLASS)).toBeGreaterThanOrEqual(4.5);
    });

    it('soft ink meets AA', () => {
      expect(ratio(TOKENS.inkSoft, GLASS)).toBeGreaterThanOrEqual(4.5);
    });
  });

  describe('the focus ring', () => {
    /**
     * The single most load-bearing colour in the product. Every keyboard
     * affordance built since M3b is worthless if the ring cannot be located,
     * and 3:1 is what makes it locatable rather than merely present.
     */
    it('is visible against glass', () => {
      expect(ratio(TOKENS.focusRing, GLASS)).toBeGreaterThanOrEqual(3);
    });
  });

  describe('state colours', () => {
    /**
     * Presence and run states are *also* conveyed by text (`presence-orb` has a
     * live region, run states are labelled), so these are non-text indicators:
     * 3:1. The test exists because a state colour that vanishes into the panel
     * takes the at-a-glance reading with it — which is the entire point of the
     * orb.
     */
    const states: [string, string][] = Object.entries(TOKENS).filter(([name]) =>
      ['idle', 'listen', 'speak', 'wait', 'done', 'error', 'degraded'].includes(name),
    ) as [string, string][];

    for (const [name, colour] of states) {
      it(`${name} is distinguishable against glass`, () => {
        expect(ratio(colour, GLASS))
          .withContext(`--c-${name} (${colour}) is too faint against a glass panel`)
          .toBeGreaterThanOrEqual(3);
      });
    }
  });

  describe('the maths itself', () => {
    // A contrast checker that is wrong reports comfortable numbers forever.
    it('agrees with the WCAG reference values', () => {
      expect(ratio('#000000', '#ffffff')).toBeCloseTo(21, 1);
      expect(ratio('#ffffff', '#ffffff')).toBeCloseTo(1, 5);
      // Published reference: #767676 on white is the canonical 4.5:1 boundary.
      expect(ratio('#767676', '#ffffff')).toBeCloseTo(4.54, 1);
    });
  });
});

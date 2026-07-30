import { BUNDLED_WALLPAPERS, GLASS_PLAIN, GLASS_WALLPAPER, glassTokensFor } from './backgrounds';
import { AA_BODY, AA_LARGE, composite, contrastRatio, parseHex } from './contrast';

/**
 * The glass-contrast audit (docs/12 §5/§9) as an executable gate.
 *
 * docs/12 §9 makes "both wallpapers pass contrast audit" an acceptance criterion
 * for HUD work. This computes it rather than eyeballing it: composite the glass
 * panel over each bundled wallpaper's **worst-case pixel**, then measure body
 * and large-caption text against WCAG AA. Passing on the extreme pixel means
 * passing everywhere on that wallpaper.
 */
describe('Glass contrast audit (docs/12 §5/§9)', () => {
  // The ink tokens from styles.scss. Kept in sync deliberately: if someone
  // lightens the ink, this audit is what fails.
  const INK = parseHex('#1c2026');
  const GLASS_WHITE = parseHex('#ffffff');

  it('has two bundled worst-case wallpapers', () => {
    expect(BUNDLED_WALLPAPERS.length).toBe(2);
    for (const wallpaper of BUNDLED_WALLPAPERS) {
      expect(wallpaper.asset).toMatch(/^backgrounds\/.+\.svg$/);
    }
  });

  it('passes AA on both wallpapers, for body and large caption text', () => {
    for (const wallpaper of BUNDLED_WALLPAPERS) {
      // The panel is white at the wallpaper-column alpha, over the worst pixel.
      const panel = composite(GLASS_WHITE, parseHex(wallpaper.extreme), GLASS_WALLPAPER.alpha);

      const body = contrastRatio(INK, panel);
      // Secondary ink comes from the same token set as the glass — that pairing
      // is the thing under audit.
      const dim = contrastRatio(parseHex(GLASS_WALLPAPER.inkDim), panel);
      const caption = contrastRatio(INK, panel);

      expect(body)
        .withContext(`body text on "${wallpaper.label}" (${body.toFixed(2)}:1)`)
        .toBeGreaterThanOrEqual(AA_BODY);
      expect(dim)
        .withContext(`secondary text on "${wallpaper.label}" (${dim.toFixed(2)}:1)`)
        .toBeGreaterThanOrEqual(AA_BODY);
      expect(caption)
        .withContext(`large caption on "${wallpaper.label}" (${caption.toFixed(2)}:1)`)
        .toBeGreaterThanOrEqual(AA_LARGE);
    }
  });

  it('passes AA with no background at all', () => {
    // The "none" column sits on the light page gradient; its lightest stop is
    // the worst case for dark ink.
    const panel = composite(GLASS_WHITE, parseHex('#e9ebef'), GLASS_PLAIN.alpha);
    expect(contrastRatio(INK, panel)).toBeGreaterThanOrEqual(AA_BODY);
    expect(contrastRatio(parseHex(GLASS_PLAIN.inkDim), panel)).toBeGreaterThanOrEqual(AA_BODY);
  });

  it('switches the whole glass token set with the background, as a unit', () => {
    // docs/12 §5: components never hand-tune. A wallpaper makes the glass
    // denser and blurrier and turns the scrim on — one switch, not per-component.
    expect(glassTokensFor('none')).toEqual(GLASS_PLAIN);
    expect(glassTokensFor('abstract')).toEqual(GLASS_WALLPAPER);
    expect(glassTokensFor('photo')).toEqual(GLASS_WALLPAPER);

    expect(GLASS_WALLPAPER.alpha).toBeGreaterThan(GLASS_PLAIN.alpha);
    expect(GLASS_PLAIN.scrim).toBe('none');
    expect(GLASS_WALLPAPER.scrim).not.toBe('none');
  });

  it('computes WCAG ratios correctly on known pairs', () => {
    // Sanity anchors: black on white is 21:1, a colour against itself is 1:1.
    expect(contrastRatio(parseHex('#000000'), parseHex('#ffffff'))).toBeCloseTo(21, 1);
    expect(contrastRatio(INK, INK)).toBeCloseTo(1, 5);
  });

  it('rejects a malformed colour rather than silently scoring it', () => {
    expect(() => parseHex('rgb(1,2,3)')).toThrowError(/expected #rrggbb/);
  });
});

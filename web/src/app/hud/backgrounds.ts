/**
 * Background feature (FR-23, docs/12 §5) and the bundled worst-case wallpapers
 * the contrast audit runs against (docs/12 §9).
 *
 * The glass system adapts **as a unit** via tokens — components never hand-tune.
 * Switching a background swaps the `--glass-*` set and turns on a full-viewport
 * scrim, exactly the two columns of the docs/12 §5 table.
 */

export type BackgroundKind = 'none' | 'abstract' | 'photo';

/** The `--glass-*` token set for one background column (docs/12 §5). */
export interface GlassTokens {
  alpha: number;
  blur: string;
  border: string;
  shadow: string;
  /** Full-viewport scrim over the wallpaper; `none` when there is no wallpaper. */
  scrim: string;
  /**
   * Secondary ink. It belongs to the glass set because contrast is a property
   * of the *pair*: over a dark wallpaper the panel lightens toward mid-grey,
   * and the plain secondary ink lands at 4.0:1 — below AA. The audit in
   * `contrast.spec.ts` is what found that, and this is the adapting token that
   * fixes it, rather than a component darkening its own text (docs/12 §5).
   */
  inkDim: string;
}

/** docs/12 §5, "No background" column. */
export const GLASS_PLAIN: GlassTokens = Object.freeze({
  alpha: 0.55,
  blur: '1.4vmin',
  border: 'rgb(255 255 255 / 0.8)',
  shadow: '0 0.4vmin 1.6vmin rgb(60 55 50 / 0.14)',
  scrim: 'none',
  inkDim: '#464d57',
});

/** docs/12 §5, "Wallpaper active" column: denser glass, deeper cool shadow, scrim. */
export const GLASS_WALLPAPER: GlassTokens = Object.freeze({
  alpha: 0.68,
  blur: '2.4vmin',
  border: 'rgb(255 255 255 / 0.55)',
  shadow: '0 0.6vmin 2.4vmin rgb(28 34 48 / 0.28)',
  scrim: 'linear-gradient(180deg, rgb(255 255 255 / 0.5) 0%, rgb(255 255 255 / 0.28) 100%)',
  inkDim: '#3a4149',
});

export function glassTokensFor(kind: BackgroundKind): GlassTokens {
  return kind === 'none' ? GLASS_PLAIN : GLASS_WALLPAPER;
}

/**
 * The two bundled worst-case wallpapers the audit must pass (docs/12 §9).
 *
 * "Worst case" is deliberate: one is near-white (the hardest case for light
 * glass — the panel nearly disappears into it), the other is a saturated dark
 * image (the hardest case for dark ink). Anything in between is easier. The
 * hex is the extreme pixel of each image, which is what the audit measures —
 * passing on the extreme means passing everywhere on that wallpaper.
 */
export interface Wallpaper {
  id: string;
  label: string;
  asset: string;
  /** The extreme (worst-case) pixel colour in the asset, as `#rrggbb`. */
  extreme: string;
}

export const BUNDLED_WALLPAPERS: readonly Wallpaper[] = Object.freeze([
  Object.freeze({
    id: 'bright-haze',
    label: 'Bright haze',
    asset: 'backgrounds/bright-haze.svg',
    extreme: '#ffffff',
  }),
  Object.freeze({
    id: 'deep-dusk',
    label: 'Deep dusk',
    asset: 'backgrounds/deep-dusk.svg',
    extreme: '#101826',
  }),
]);

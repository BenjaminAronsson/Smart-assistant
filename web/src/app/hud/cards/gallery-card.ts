import { ChangeDetectionStrategy, Component, input } from '@angular/core';
import type { HudCardDto } from '../../../generated/api-types';
import { SourcedImage } from './sourced-image';

/** Wire shape of this card's own variant, narrowed once by `hud-card`. */
export type GalleryCardData = Extract<HudCardDto, { type: 'card.gallery' }>;

/**
 * Gallery card (docs/12 §2.3, FR-27/ADR-017): a small image grid, **each tile
 * individually source-badged**.
 *
 * Per-tile attribution is not a styling choice here — it is the only thing this
 * component can do. Tiles are rendered by `app-sourced-image`, the single
 * renderer for `SourcedImageDto`, which always paints the image's own chip and
 * alt text alongside it. There is no card-level source input to fall back on, so
 * "one shared link across images from different pages" (which ADR-017 forbids)
 * is unrepresentable rather than merely discouraged.
 *
 * The 6–8 cap is enforced server-side (`GALLERY_IMAGE_CAP`) because it is a
 * tool-call budget decision, not a layout one.
 */
@Component({
  selector: 'app-gallery-card',
  imports: [SourcedImage],
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './gallery-card.html',
  styleUrl: './gallery-card.scss',
})
export class GalleryCard {
  readonly card = input.required<GalleryCardData>();
}

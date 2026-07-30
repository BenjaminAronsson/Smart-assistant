import { ChangeDetectionStrategy, Component, input } from '@angular/core';
import type { HudCardDto } from '../../../generated/api-types';
import { SourcedImage } from './sourced-image';

/** Wire shape of this card's own variant, narrowed once by `hud-card`. */
export type MediaGridCardData = Extract<HudCardDto, { type: 'card.media_grid' }>;

/**
 * Media/menu grid card (docs/12 §2.3: "photo + name + price"). Each tile's
 * photo, when present, is a full [`SourcedImage`] — no tile can show a web
 * photo without its own chip.
 */
@Component({
  selector: 'app-media-grid-card',
  imports: [SourcedImage],
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './media-grid-card.html',
  styleUrl: './media-grid-card.scss',
})
export class MediaGridCard {
  readonly card = input.required<MediaGridCardData>();
}

import { ChangeDetectionStrategy, Component, input } from '@angular/core';
import type { HudCardDto } from '../../../generated/api-types';
import { SourcedImage } from './sourced-image';

/** Wire shape of this card's own variant, narrowed once by `hud-card`. */
export type PlaceCardData = Extract<HudCardDto, { type: 'card.place' }>;

/**
 * Place result (docs/12 §2.3): photo, rating/distance/price pills, and the
 * `pick` variant's hue ring marking a top recommendation. The photo, when
 * present, is always a full [`SourcedImage`] — there is no field here for a
 * bare image URL, so a place card cannot show a web photo without its chip.
 */
@Component({
  selector: 'app-place-card',
  imports: [SourcedImage],
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './place-card.html',
  styleUrl: './place-card.scss',
  host: {
    '[class.pick]': 'card().pick',
  },
})
export class PlaceCard {
  readonly card = input.required<PlaceCardData>();
}

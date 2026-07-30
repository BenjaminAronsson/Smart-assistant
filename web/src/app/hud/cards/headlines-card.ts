import { ChangeDetectionStrategy, Component, input } from '@angular/core';
import type { HudCardDto } from '../../../generated/api-types';
import { SourceChip } from './source-chip';
import { SourcedImage } from './sourced-image';

/** Wire shape of this card's own variant, narrowed once by `hud-card`. */
export type HeadlinesCardData = Extract<HudCardDto, { type: 'card.headlines' }>;

/**
 * Headlines/digest card (docs/12 §2.3): several current items, not one fact —
 * each with its own source link, independent of any thumbnail's attribution
 * (a digest's items may each come from a different page).
 */
@Component({
  selector: 'app-headlines-card',
  imports: [SourceChip, SourcedImage],
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './headlines-card.html',
  styleUrl: './headlines-card.scss',
})
export class HeadlinesCard {
  readonly card = input.required<HeadlinesCardData>();
}

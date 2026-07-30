import { ChangeDetectionStrategy, Component, input } from '@angular/core';
import type { HudCardDto } from '../../../generated/api-types';
import { SourcedImage } from './sourced-image';

/** Wire shape of this card's own variant, narrowed once by `hud-card`. */
export type EntityCardData = Extract<HudCardDto, { type: 'card.entity' }>;

/**
 * Entity/person card (docs/12 §2.3): photo, confidence, facts. `facts` is a
 * list of plain sentences — rendered one per line via interpolation, never
 * markup.
 */
@Component({
  selector: 'app-entity-card',
  imports: [SourcedImage],
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './entity-card.html',
  styleUrl: './entity-card.scss',
})
export class EntityCard {
  readonly card = input.required<EntityCardData>();
}

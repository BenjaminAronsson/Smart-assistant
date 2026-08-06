import { ChangeDetectionStrategy, Component, input } from '@angular/core';
import type { HudCardDto } from '../../../generated/api-types';

/** Wire shape of the read-only, sensitivity-safe agenda card. */
export type AgendaCardData = Extract<HudCardDto, { type: 'card.agenda' }>;

/**
 * Renders the bounded calendar projection. The component deliberately has no
 * calendar actions and interpolates every field as text; provider details and
 * sensitivity labels never reach this surface.
 */
@Component({
  selector: 'app-agenda-card',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './agenda-card.html',
  styleUrl: './agenda-card.scss',
})
export class AgendaCard {
  readonly card = input.required<AgendaCardData>();
}

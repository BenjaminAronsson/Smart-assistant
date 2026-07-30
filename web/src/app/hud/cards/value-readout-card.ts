import { ChangeDetectionStrategy, Component, input } from '@angular/core';
import type { HudCardDto } from '../../../generated/api-types';

/** Wire shape of this card's own variant, narrowed once by `hud-card`. */
export type ValueReadoutCardData = Extract<HudCardDto, { type: 'card.value_readout' }>;

/**
 * Hero number readout (docs/12 §2.3): a single value plus optional staggered
 * mini-stats. The server sends the value pre-formatted (`"72°F"`); this
 * component only applies the tabular-nums presentation, it never computes a
 * number from text — every field is rendered as plain interpolated text.
 */
@Component({
  selector: 'app-value-readout-card',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './value-readout-card.html',
  styleUrl: './value-readout-card.scss',
})
export class ValueReadoutCard {
  readonly card = input.required<ValueReadoutCardData>();
}

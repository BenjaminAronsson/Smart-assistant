import { ChangeDetectionStrategy, Component, input } from '@angular/core';
import type { HudCardDto } from '../../../generated/api-types';

/** Wire shape of this card's own variant, narrowed once by `hud-card`. */
export type StatusCardData = Extract<HudCardDto, { type: 'card.status' }>;

/**
 * Status/queued card (docs/12 §2.3): a transient readout, e.g. a run parked
 * in degraded-mode queueing (FR-12). `message` is plain text.
 */
@Component({
  selector: 'app-status-card',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './status-card.html',
  styleUrl: './status-card.scss',
  host: {
    role: 'status',
  },
})
export class StatusCard {
  readonly card = input.required<StatusCardData>();
}

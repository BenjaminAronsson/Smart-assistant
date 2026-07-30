import { ChangeDetectionStrategy, Component, input, output } from '@angular/core';
import type { ApprovalCardDto, ApprovalDecisionDto } from '../../../generated/api-types';
import { ApprovalTray } from '../../approval-tray';

/**
 * The approval surface as a materialization-canvas card (docs/12 §2.3): a
 * thin wrapper around the existing `ApprovalTray` (F2.5) — the card grammar
 * reuses that component and its DTO verbatim rather than re-modeling the
 * approval anatomy a second time. Approval cards are exempt from the
 * shelve/dismiss/TTL lifecycle (F3b.4); this component only renders.
 */
@Component({
  selector: 'app-approval-card',
  imports: [ApprovalTray],
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './approval-card.html',
})
export class ApprovalCard {
  readonly card = input.required<ApprovalCardDto>();
  readonly pending = input(false);
  readonly decide = output<ApprovalDecisionDto>();
}

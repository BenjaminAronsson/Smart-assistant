import type { HudCardDto } from '../../../generated/api-types';

/**
 * A stable key for `@for … track` over a card list. Every variant carries its
 * own `id` except `card.approval`, which wire-reuses `ApprovalCardDto` and so
 * keys off its `approvalId` instead (docs/12 §2.3 — the approval surface is
 * never re-modeled, not even for a synthetic id).
 */
export function hudCardId(card: HudCardDto): string {
  return card.type === 'card.approval' ? card.card.approvalId : card.id;
}

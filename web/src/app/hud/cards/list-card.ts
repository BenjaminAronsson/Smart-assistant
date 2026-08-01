import { ChangeDetectionStrategy, Component, computed, input, output } from '@angular/core';
import type { HudCardDto, ListItemDto, UlidString } from '../../../generated/api-types';

/** Wire shape of this card's own variant, narrowed once by `hud-card`. */
export type ListCardData = Extract<HudCardDto, { type: 'card.list' }>;

/** What a check-off tap asks the host to do — the body of
 * `PATCH /api/v1/lists/{listId}/items/{itemId}`, plus the ids needed to
 * address that call. */
export interface ListItemCheckIntent {
  listId: UlidString;
  itemId: UlidString;
  checked: boolean;
}

/**
 * List card (docs/12 §2.3, FR-34/ADR-024): a named list with tap check-off.
 *
 * Pure presentational, the same division of labor as `TimerCard`: it renders
 * the list it is given and emits the owner's intent, nothing more. The host
 * owns the `PATCH` call and the WS-driven re-render that follows it — a tap
 * here is never itself the authority that changes stored state (invariant
 * 1), only a request that the list card re-renders once the server confirms
 * it.
 *
 * `name` and every item's `text` are sanitized human text (per
 * `ListDto`/`ListItemDto`'s own doc comments) and are rendered as plain
 * interpolation only — the card grammar carries no model-authored HTML
 * (docs/12 §9).
 */
@Component({
  selector: 'app-list-card',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './list-card.html',
  styleUrl: './list-card.scss',
})
export class ListCard {
  readonly card = input.required<ListCardData>();
  /** A check-off is in flight. All items block until it resolves — one flag
   * for the whole card, the same granularity `TimerCard`'s `pending` uses,
   * since the card has no per-row in-flight state to track. */
  readonly pending = input(false);
  readonly checkItem = output<ListItemCheckIntent>();

  /** "3 left" / "All done" — blank for an empty list, which already says so
   * in the body. Matches the spoken readback so the card and the voice
   * answer never disagree (the server computes `openCount` for exactly this
   * reason). */
  protected readonly openLabel = computed(() => {
    const list = this.card().list;
    if (list.items.length === 0) return '';
    return list.openCount === 0 ? 'All done' : `${list.openCount} left`;
  });

  protected onToggle(item: ListItemDto): void {
    if (this.pending()) return;
    this.checkItem.emit({ listId: this.card().listId, itemId: item.id, checked: !item.checked });
  }
}

import { ChangeDetectionStrategy, Component, input } from '@angular/core';
import type { HudCardDto } from '../../../generated/api-types';
import { SourceChip } from './source-chip';

/** Wire shape of this card's own variant, narrowed once by `hud-card`. */
export type SourcesCardData = Extract<HudCardDto, { type: 'card.sources' }>;

/**
 * Sources card (docs/12 §2.3/§2.5, FR-27/ADR-017) — the bibliography for "show
 * me the references": each consulted page as a title, a domain chip and a link.
 *
 * It is a **list of references, not a reader**. There is no page body here and
 * no wire field that could carry one: reading a source opens the real page in
 * the browser worker (ADR-017 §3), which is a scope and a copyright boundary
 * both. `domain` is rendered as the server computed it — the client never
 * derives a trusted-looking label from `url` itself.
 */
@Component({
  selector: 'app-sources-card',
  imports: [SourceChip],
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './sources-card.html',
  styleUrl: './sources-card.scss',
})
export class SourcesCard {
  readonly card = input.required<SourcesCardData>();
}

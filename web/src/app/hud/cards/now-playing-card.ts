import { ChangeDetectionStrategy, Component, input } from '@angular/core';
import type { HudCardDto } from '../../../generated/api-types';

/** Wire shape of this card's own variant, narrowed once by `hud-card`. */
export type NowPlayingCardData = Extract<HudCardDto, { type: 'card.now_playing' }>;

/**
 * "What's playing" as a first-class query (docs/12 §2.3, FR-32/ADR-022):
 * **data only** — no transport controls here, those stay on the media bar
 * until M5. Art is the player's own content, not third-party web content, so
 * it renders with no source chip — matching the media bar's existing
 * treatment (`media-bar.html`).
 */
@Component({
  selector: 'app-now-playing-card',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './now-playing-card.html',
  styleUrl: './now-playing-card.scss',
})
export class NowPlayingCard {
  readonly card = input.required<NowPlayingCardData>();
}

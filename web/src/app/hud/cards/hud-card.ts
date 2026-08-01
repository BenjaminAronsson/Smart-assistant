import { ChangeDetectionStrategy, Component, computed, input, output } from '@angular/core';
import type { ApprovalDecisionDto, HudCardDto } from '../../../generated/api-types';
import { ApprovalCard } from './approval-card';
import { EntityCard } from './entity-card';
import { ErrorCard } from './error-card';
import { GalleryCard } from './gallery-card';
import { HeadlinesCard } from './headlines-card';
import { ListCard, type ListItemCheckIntent } from './list-card';
import { MapCard } from './map-card';
import { MediaGridCard } from './media-grid-card';
import { NowPlayingCard } from './now-playing-card';
import { PlaceCard } from './place-card';
import { SourcesCard } from './sources-card';
import { StatusCard } from './status-card';
import { ValueReadoutCard } from './value-readout-card';

/** Narrow a `HudCardDto` to one variant, or `null` when it does not match. */
function narrow<T extends HudCardDto['type']>(
  card: HudCardDto,
  type: T,
): Extract<HudCardDto, { type: T }> | null {
  return card.type === type ? (card as Extract<HudCardDto, { type: T }>) : null;
}

/**
 * The card-grammar switch (docs/12 §2.3/§9): renders **registered card types
 * only**. A discriminant this switch does not recognize — a future contract
 * version's card, a malformed payload — degrades to the error card, never to
 * raw content. This is the client-side half of the security property whose
 * server-side half is `HudCardDto` staying the single source of truth for
 * what "registered" means (`jarvis_contracts::cards`).
 *
 * Presentational: it narrows `card()` to one of the sub-components below and
 * forwards the reveal animation's stagger index and the reduced-motion flag.
 * It owns no state.
 */
@Component({
  selector: 'app-hud-card',
  imports: [
    ValueReadoutCard,
    PlaceCard,
    EntityCard,
    MediaGridCard,
    HeadlinesCard,
    NowPlayingCard,
    MapCard,
    SourcesCard,
    GalleryCard,
    ListCard,
    ApprovalCard,
    StatusCard,
    ErrorCard,
  ],
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './hud-card.html',
  styleUrl: './hud-card.scss',
  host: {
    class: 'hud-card',
    '[style.--card-index]': 'index()',
    '[class.reduced-motion]': 'reducedMotion()',
    '[attr.data-card-type]': 'card().type',
  },
})
export class HudCard {
  readonly card = input.required<HudCardDto>();
  /** Position in the current canvas — the reveal's ~120ms stagger index. */
  readonly index = input(0);
  readonly reducedMotion = input(false);
  /** A decision for the approval variant, forwarded from the tray. */
  readonly approvalPending = input(false);
  readonly approvalDecide = output<ApprovalDecisionDto>();
  /** A check-off is in flight for the list variant, forwarded from the host. */
  readonly listPending = input(false);
  readonly listCheckItem = output<ListItemCheckIntent>();

  protected readonly asValueReadout = computed(() => narrow(this.card(), 'card.value_readout'));
  protected readonly asPlace = computed(() => narrow(this.card(), 'card.place'));
  protected readonly asEntity = computed(() => narrow(this.card(), 'card.entity'));
  protected readonly asMediaGrid = computed(() => narrow(this.card(), 'card.media_grid'));
  protected readonly asHeadlines = computed(() => narrow(this.card(), 'card.headlines'));
  protected readonly asNowPlaying = computed(() => narrow(this.card(), 'card.now_playing'));
  protected readonly asMap = computed(() => narrow(this.card(), 'card.map'));
  protected readonly asSources = computed(() => narrow(this.card(), 'card.sources'));
  protected readonly asGallery = computed(() => narrow(this.card(), 'card.gallery'));
  protected readonly asList = computed(() => narrow(this.card(), 'card.list'));
  protected readonly asApproval = computed(() => narrow(this.card(), 'card.approval'));
  protected readonly asStatus = computed(() => narrow(this.card(), 'card.status'));
  protected readonly asError = computed(() => narrow(this.card(), 'card.error'));
}

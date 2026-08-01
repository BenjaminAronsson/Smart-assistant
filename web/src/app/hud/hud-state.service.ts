import { Injectable, computed, signal } from '@angular/core';
import type { HudCardDto } from '../../generated/api-types';
import { hudCardId } from './cards/card-id';
import { type BackgroundKind, type GlassTokens, glassTokensFor } from './backgrounds';

/** The shelf holds at most 4 panels; the oldest drops (docs/12 §4). */
const MAX_SHELF = 4;

/** A canvas set collapsed into a labeled shelf chip (docs/12 §4). */
export interface ShelvedPanel {
  id: string;
  /** Chip label, e.g. "Ramen places". */
  label: string;
  cards: HudCardDto[];
  shelvedAt: number;
}

/** Approvals are exempt from shelving, clear-all and TTL (docs/12 §4). */
export function isApproval(card: HudCardDto): boolean {
  return card.type === 'card.approval';
}

/**
 * What the deep-dive router decided this turn should do to the canvas (FR-27,
 * ADR-017, docs/12 §2.5) — the client mirror of the server's `CanvasAction`.
 *
 * A continuation *extends*; only a genuine topic change shelves. The decision
 * is made server-side (`jarvis_application::deepdive`), never re-derived here
 * from the utterance — there is one classifier, and it is the one that can be
 * corrected by voice.
 */
export type CanvasAction = 'extend' | 'shelve';

/**
 * Presence states (docs/12 §2.1). Exhaustive on purpose — a new state needs a
 * hue **and** a motion signature, because colour alone fails accessibility.
 */
export type PresenceState =
  | 'idle'
  | 'listening'
  | 'speaking'
  | 'tool'
  | 'waiting'
  | 'done'
  | 'error'
  | 'degraded';

/**
 * Hue token per state (docs/12 §2.1). This map is the enforcement point for
 * **amber exclusivity**: `--c-wait` appears here exactly once, against
 * `waiting`, and nothing else in the HUD picks a hue by hand.
 */
export const PRESENCE_HUE: Readonly<Record<PresenceState, string>> = Object.freeze({
  idle: '--c-idle',
  listening: '--c-listen',
  speaking: '--c-speak',
  tool: '--c-tool',
  waiting: '--c-wait',
  done: '--c-done',
  error: '--c-error',
  degraded: '--c-degraded',
});

/** Spoken state name for the aria-live announcement (docs/12 §8). */
export const PRESENCE_LABEL: Readonly<Record<PresenceState, string>> = Object.freeze({
  idle: 'Idle',
  listening: 'Listening',
  speaking: 'Speaking',
  tool: 'Running a tool',
  waiting: 'Waiting on you',
  done: 'Done',
  error: 'Error',
  degraded: 'Degraded',
});

/** A sentence-splitter for the caption reveal. Voice timing marks replace this
 * in M5; until then the caption reveals per sentence, never a fake typewriter
 * slower than speech (docs/12 §2.2). */
function sentences(text: string): string[] {
  return text
    .split(/(?<=[.!?])\s+/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

/**
 * HUD face state (docs/12 §1/§2/§6): what the orb shows, what the caption says,
 * whether the ops layer is open, and whether ambient motion may run.
 *
 * Deliberately a plain signal service (docs/08 §6: signals + services, no NgRx
 * without an ADR). Later M3b features push into it — F3b.2 cards, F3b.6
 * deep-dive threads — rather than owning their own presence notion.
 */
@Injectable({ providedIn: 'root' })
export class HudStateService {
  private readonly presenceSignal = signal<PresenceState>('idle');
  private readonly captionSignal = signal('');
  private readonly revealedSignal = signal(0);
  private readonly opsOpenSignal = signal(false);
  private readonly windowActiveSignal = signal(true);
  private readonly reducedMotionSignal = signal(false);
  /** Set by the low-power profile (docs/12 §6). No browser API reports the OS
   * battery-saver reliably, so it is pushed in rather than sniffed. */
  private readonly batterySaverSignal = signal(false);
  /**
   * The materialization canvas's cards (docs/12 §2.3, F3b.2). No producer
   * wires this from the wire yet — F3b.2 ships the grammar and renderers with
   * no server-side event (see `jarvis_contracts::cards`'s doc comment); the
   * first producer is F3b.6. Panel lifecycle (shelve/restore/dismiss/TTL) is
   * F3b.4 — this service only holds the current list.
   */
  private readonly cardsSignal = signal<HudCardDto[]>([]);
  /** Shelved panel sets, oldest first (docs/12 §4, F3b.4). */
  private readonly shelfSignal = signal<ShelvedPanel[]>([]);
  private readonly panelTtlHoursSignal = signal(2);
  private readonly backgroundSignal = signal<BackgroundKind>('none');
  private readonly backgroundAssetSignal = signal<string | null>(null);

  /** When each card first appeared, for the silent TTL sweep. */
  private readonly seenAt = new Map<string, number>();
  private shelfSeq = 0;
  /** Label for whatever is currently on the canvas, used when a restore swaps. */
  private lastCanvasLabel = 'Previous';

  private revealTimer: ReturnType<typeof setInterval> | null = null;

  readonly presence = this.presenceSignal.asReadonly();
  readonly caption = this.captionSignal.asReadonly();
  readonly opsOpen = this.opsOpenSignal.asReadonly();
  readonly cards = this.cardsSignal.asReadonly();
  readonly shelf = this.shelfSignal.asReadonly();
  readonly background = this.backgroundSignal.asReadonly();
  readonly backgroundAsset = this.backgroundAssetSignal.asReadonly();

  /** The `--glass-*` set for the active background (docs/12 §5) — one switch
   * for the whole system, so no component hand-tunes for a wallpaper. */
  readonly glass = computed<GlassTokens>(() => glassTokensFor(this.backgroundSignal()));

  /** The hue token the orb (and only the orb) paints itself with. */
  readonly hue = computed(() => PRESENCE_HUE[this.presenceSignal()]);
  /** The state name a screen reader announces (docs/12 §8). */
  readonly presenceLabel = computed(() => PRESENCE_LABEL[this.presenceSignal()]);

  /** Sentences revealed so far — the caption's visible text. */
  readonly captionSentences = computed(() =>
    sentences(this.captionSignal()).slice(0, this.revealedSignal()),
  );

  /**
   * Ambient motion gate (docs/12 §6): ring spin, breathe and particles stop when
   * the window is hidden or unfocused, under `prefers-reduced-motion`, or in
   * battery-saver. Event animations are handled in CSS, which reduces them to
   * ~1ms under the same media query.
   */
  readonly ambientMotion = computed(
    () => this.windowActiveSignal() && !this.reducedMotionSignal() && !this.batterySaverSignal(),
  );

  readonly reducedMotion = this.reducedMotionSignal.asReadonly();

  setPresence(state: PresenceState): void {
    this.presenceSignal.set(state);
  }

  /**
   * Say one utterance. It replaces the previous one (docs/12 §2.2: one utterance
   * at a time; the full transcript lives in the ops layer) and reveals sentence
   * by sentence unless motion is reduced, in which case it lands at once.
   */
  speak(text: string, sentenceIntervalMs = 900): void {
    this.stopReveal();
    this.captionSignal.set(text);
    const total = sentences(text).length;
    if (total === 0) {
      this.revealedSignal.set(0);
      return;
    }
    if (this.reducedMotionSignal() || total === 1) {
      this.revealedSignal.set(total);
      return;
    }
    this.revealedSignal.set(1);
    this.revealTimer = setInterval(() => {
      const next = this.revealedSignal() + 1;
      this.revealedSignal.set(next);
      if (next >= total) {
        this.stopReveal();
      }
    }, sentenceIntervalMs);
  }

  toggleOps(): void {
    this.opsOpenSignal.update((open) => !open);
  }

  setOpsOpen(open: boolean): void {
    this.opsOpenSignal.set(open);
  }

  setWindowActive(active: boolean): void {
    this.windowActiveSignal.set(active);
  }

  setReducedMotion(reduced: boolean): void {
    this.reducedMotionSignal.set(reduced);
  }

  setBatterySaver(saving: boolean): void {
    this.batterySaverSignal.set(saving);
  }

  /**
   * Replace the canvas — **pending approvals survive** (docs/12 §4).
   *
   * A bulk replace that dropped an undecided approval would silently discard a
   * decision the human still owes, so approvals already on the canvas are
   * carried over unless the new set supersedes them by id. This is the same
   * exemption `newQuery` and `clearAll` honour; keeping it here means no
   * producer can lose an approval by accident.
   */
  setCards(cards: HudCardDto[]): void {
    const incomingIds = new Set(cards.map(hudCardId));
    const survivingApprovals = this.cardsSignal().filter(
      (card) => isApproval(card) && !incomingIds.has(hudCardId(card)),
    );
    const next = [...survivingApprovals, ...cards];
    this.cardsSignal.set(next);
    this.touchCards(next);
  }

  /** Extend the live canvas — a continuation appends, it never shelves (FR-24,
   * docs/12 §2.5). */
  appendCards(cards: HudCardDto[]): void {
    this.cardsSignal.update((existing) => [...existing, ...cards]);
    this.touchCards(cards);
  }

  /**
   * Apply one deep-dive turn's canvas decision (F3b.6, FR-27/ADR-017).
   *
   * This is the single place the continuation signal meets the panel lifecycle,
   * so there is exactly one answer to "does a follow-up shelve?": a
   * `'extend'` appends to the live canvas and leaves prior cards in place; only
   * a `'shelve'` collapses them into a labeled chip (FR-24, unchanged for that
   * case). Pending approvals are exempt from either — both paths below go
   * through [`newQuery`]/[`setCards`], which carry them over rather than
   * dropping a decision the human still owes.
   */
  routeTurn(action: CanvasAction, label: string, cards: HudCardDto[]): void {
    if (action === 'extend') {
      this.appendCards(cards);
      return;
    }
    this.newQuery(label);
    this.setCards(cards);
  }

  // --- panel lifecycle (FR-24, docs/12 §4) --------------------------------

  /**
   * A new **topic** arrives: the current canvas collapses into a labeled shelf
   * chip and the canvas starts empty (docs/12 §4).
   *
   * Two rules that are easy to get wrong and are therefore tested: pending
   * **approval cards are exempt** — they stay on the canvas and are never
   * shelved, because they are the human's job and a new question does not
   * retract them — and the shelf holds **at most 4**, oldest dropped.
   *
   * A continuation must call [`appendCards`] instead; deciding which is which is
   * the router's job in F3b.6, not this service's.
   */
  newQuery(label: string, now = Date.now()): void {
    const current = this.cardsSignal();
    const approvals = current.filter(isApproval);
    const shelvable = current.filter((card) => !isApproval(card));

    if (shelvable.length > 0) {
      const entry: ShelvedPanel = {
        id: `shelf-${now}-${this.shelfSeq++}`,
        label,
        cards: shelvable,
        shelvedAt: now,
      };
      this.shelfSignal.update((shelf) => [...shelf, entry].slice(-MAX_SHELF));
    }
    // Approvals survive the topic change; everything else is now on the shelf.
    this.cardsSignal.set(approvals);
  }

  /**
   * Swap a shelved set back onto the canvas, shelving what was there (docs/12
   * §4). Pending approvals stay put through the swap, same as [`newQuery`].
   */
  restore(shelfId: string, now = Date.now()): void {
    const entry = this.shelfSignal().find((panel) => panel.id === shelfId);
    if (!entry) {
      return;
    }
    const current = this.cardsSignal();
    const approvals = current.filter(isApproval);
    const displaced = current.filter((card) => !isApproval(card));

    this.shelfSignal.update((shelf) => {
      const rest = shelf.filter((panel) => panel.id !== shelfId);
      if (displaced.length === 0) {
        return rest;
      }
      const swapped: ShelvedPanel = {
        id: `shelf-${now}-${this.shelfSeq++}`,
        label: entry.label === this.lastCanvasLabel ? 'Previous' : this.lastCanvasLabel,
        cards: displaced,
        shelvedAt: now,
      };
      return [...rest, swapped].slice(-MAX_SHELF);
    });
    this.lastCanvasLabel = entry.label;
    this.cardsSignal.set([...approvals, ...entry.cards]);
    this.touchCards(entry.cards, now);
  }

  /** Dismiss one card from the canvas (the per-card `×`, docs/12 §4). */
  dismissCard(cardId: string): void {
    this.cardsSignal.update((cards) => cards.filter((card) => hudCardId(card) !== cardId));
  }

  /** Dismiss one shelf chip (its `×`, docs/12 §4). */
  dismissShelf(shelfId: string): void {
    this.shelfSignal.update((shelf) => shelf.filter((panel) => panel.id !== shelfId));
  }

  /**
   * "Clear all" (docs/12 §4) — canvas and shelf, **except pending approvals**,
   * which a bulk action must not silently drop.
   */
  clearAll(): void {
    this.cardsSignal.update((cards) => cards.filter(isApproval));
    this.shelfSignal.set([]);
  }

  /**
   * Silent TTL expiry (docs/12 §4: "expiry is silent — no animation, no
   * notification"). Displayed and shelved panels older than `panel_ttl_hours`
   * (docs/09 §1, default 2) simply stop being there. Approvals are exempt: they
   * persist until decided or until their own grant expires.
   *
   * Takes `now` so the sweep is deterministic in tests; the view drives it on a
   * timer.
   */
  sweepExpired(now = Date.now()): void {
    const cutoff = now - this.ttlMs();
    this.cardsSignal.update((cards) =>
      cards.filter((card) => isApproval(card) || (this.seenAt.get(hudCardId(card)) ?? now) > cutoff),
    );
    this.shelfSignal.update((shelf) => shelf.filter((panel) => panel.shelvedAt > cutoff));
  }

  /** `[ui] panel_ttl_hours` (docs/09 §1, default 2). */
  setPanelTtlHours(hours: number): void {
    if (Number.isFinite(hours) && hours > 0) {
      this.panelTtlHoursSignal.set(hours);
    }
  }

  private ttlMs(): number {
    return this.panelTtlHoursSignal() * 60 * 60 * 1000;
  }

  /** Remember when each card first appeared, for the TTL sweep. */
  private touchCards(cards: HudCardDto[], now = Date.now()): void {
    for (const card of cards) {
      const id = hudCardId(card);
      if (!this.seenAt.has(id)) {
        this.seenAt.set(id, now);
      }
    }
  }

  // --- background (FR-23, docs/12 §5) -------------------------------------

  /**
   * Switch the background. The `--glass-*` token set moves with it as a unit —
   * that is the whole point of the token system (docs/12 §5); no component
   * adjusts itself for a wallpaper.
   */
  setBackground(kind: BackgroundKind, photoAsset?: string): void {
    this.backgroundSignal.set(kind);
    this.backgroundAssetSignal.set(photoAsset ?? null);
  }

  /** Stop the reveal timer — called on teardown so no timer outlives the view. */
  stopReveal(): void {
    if (this.revealTimer !== null) {
      clearInterval(this.revealTimer);
      this.revealTimer = null;
    }
  }
}

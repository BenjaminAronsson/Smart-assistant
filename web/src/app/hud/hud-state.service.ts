import { Injectable, computed, signal } from '@angular/core';
import type { HudCardDto } from '../../generated/api-types';

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

  private revealTimer: ReturnType<typeof setInterval> | null = null;

  readonly presence = this.presenceSignal.asReadonly();
  readonly caption = this.captionSignal.asReadonly();
  readonly opsOpen = this.opsOpenSignal.asReadonly();
  readonly cards = this.cardsSignal.asReadonly();

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

  /** Replace the canvas outright — a new topic shelves the old set (F3b.4). */
  setCards(cards: HudCardDto[]): void {
    this.cardsSignal.set(cards);
  }

  /** Extend the live canvas — a continuation appends, it never shelves (FR-24,
   * docs/12 §2.5; the continuation-vs-new-topic router lands in F3b.6). */
  appendCards(cards: HudCardDto[]): void {
    this.cardsSignal.update((existing) => [...existing, ...cards]);
  }

  /** Stop the reveal timer — called on teardown so no timer outlives the view. */
  stopReveal(): void {
    if (this.revealTimer !== null) {
      clearInterval(this.revealTimer);
      this.revealTimer = null;
    }
  }
}

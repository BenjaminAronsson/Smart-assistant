import {
  ChangeDetectionStrategy,
  Component,
  DestroyRef,
  computed,
  effect,
  inject,
  input,
  output,
  signal,
  untracked,
} from '@angular/core';
import type { TimerDto } from '../../../generated/api-types';

/** How often the countdown redraws. One second is the smallest visible step. */
const TICK_MS = 1000;

/**
 * Timer / reminder card (FR-33, ADR-023, docs/12 §2.3).
 *
 * The visible half of a timer. Three jobs, and it does no more than these:
 *
 * - **Count down live.** The server sends `remainingSecs` once, at a stated
 *   `now`; the card ticks locally from there rather than polling (docs/09 §5 —
 *   no network loop for something a clock can do). The tick is cleared on
 *   destroy and, per docs/12 §6, it stops while the document is hidden: a
 *   background tab must not keep a repaint loop alive on a battery.
 * - **Say what state it is in, in words.** A ringing timer is announced through
 *   `aria-live`, not signalled by colour or motion alone (docs/12 §8). A timer
 *   that went off while Jarvis was down carries a visible "missed while
 *   offline" notice — ADR-023 requires the human be told, not left to infer it
 *   from a stale time.
 * - **Offer exactly the affordances the state allows.** Armed ⇒ cancel.
 *   Ringing ⇒ dismiss or snooze. Nothing else is rendered, so the card can
 *   never present a control the server would refuse.
 *
 * It is a pure presentational component: it renders the timer it is given and
 * emits intents. The host owns the POST and the WS subscription. `name` and
 * `note` are human text sanitized server-side and interpolated as **text only**,
 * never markup — the card grammar carries no model-authored HTML (docs/12 §9).
 */
@Component({
  selector: 'app-timer-card',
  standalone: true,
  templateUrl: './timer-card.html',
  styleUrl: './timer-card.scss',
  changeDetection: ChangeDetectionStrategy.OnPush,
  host: {
    '[class]': '"timer-card state-" + timer().state',
  },
})
export class TimerCard {
  /** The timer to render, as the server last projected it. */
  readonly timer = input.required<TimerDto>();
  /**
   * It came due while jarvisd was not running (the `timer.fired` event's
   * `missed` flag). Shown as a notice — never silently folded into a normal
   * ring (ADR-023).
   */
  readonly missed = input(false);
  /** An action is in flight — the controls block until it resolves. */
  readonly pending = input(false);

  readonly dismiss = output<string>();
  readonly snooze = output<string>();
  /**
   * Named `cancelTimer` rather than `cancel`: `cancel` is a native DOM event
   * name, and an output that shadows one is ambiguous at every call site
   * (`@angular-eslint/no-output-native`).
   */
  readonly cancelTimer = output<string>();

  private readonly destroyRef = inject(DestroyRef);
  /** Seconds left, ticking locally from the server's value. */
  private readonly remaining = signal<number | null>(null);
  private handle: ReturnType<typeof setInterval> | null = null;
  /** Mirrors `armed()` outside the reactive graph, for the tick and the
   * visibilitychange handler (see the `untracked` note below). */
  private ticking = false;

  constructor() {
    // Re-seed from the server whenever a fresh projection arrives, so the card
    // converges on server truth instead of drifting on its own arithmetic.
    //
    // The write and the re-tick are `untracked`: they read `remaining`, and an
    // effect that both reads and writes the same signal re-runs on its own
    // write — which would reset the countdown to the server value every tick.
    effect(() => {
      const timer = this.timer();
      const secs = timer.remainingSecs;
      const armed = timer.state === 'pending' || timer.state === 'snoozed';
      untracked(() => {
        this.remaining.set(typeof secs === 'number' ? Math.max(0, Math.trunc(secs)) : null);
        this.ticking = armed;
        this.retick();
      });
    });
    document.addEventListener('visibilitychange', this.onVisibility);
    this.destroyRef.onDestroy(() => {
      this.stop();
      document.removeEventListener('visibilitychange', this.onVisibility);
    });
  }

  /** True while the timer is counting down and can be called off. */
  protected readonly armed = computed(
    () => this.timer().state === 'pending' || this.timer().state === 'snoozed',
  );

  /** True while it is going off and waiting for an answer. */
  protected readonly ringing = computed(() => this.timer().state === 'fired');

  /** `9:59`, `1:02:03`, or `0:00` once it is up. Null when there is nothing to count. */
  protected readonly countdown = computed(() => {
    const secs = this.remaining();
    if (secs === null) return null;
    const hours = Math.floor(secs / 3600);
    const minutes = Math.floor((secs % 3600) / 60);
    const seconds = secs % 60;
    const mm = hours > 0 ? String(minutes).padStart(2, '0') : String(minutes);
    return `${hours > 0 ? `${hours}:` : ''}${mm}:${String(seconds).padStart(2, '0')}`;
  });

  /**
   * The spoken-equivalent status line. This is what a screen reader announces,
   * so it must carry the whole meaning of the card on its own (docs/12 §8).
   */
  protected readonly status = computed(() => {
    const timer = this.timer();
    if (this.ringing()) {
      const what = timer.note ? `Reminder — ${timer.note}` : `${timer.name} is up`;
      return this.missed() ? `Missed while offline. ${what}` : what;
    }
    switch (timer.state) {
      case 'pending':
      case 'snoozed': {
        const left = this.countdown();
        const prefix = timer.state === 'snoozed' ? 'Snoozed. ' : '';
        return left === null ? `${prefix}${timer.name}` : `${prefix}${timer.name}, ${left} left`;
      }
      case 'dismissed':
        return `${timer.name} — dismissed`;
      case 'cancelled':
        return `${timer.name} — cancelled`;
      default:
        return timer.name;
    }
  });

  protected onDismiss(): void {
    if (!this.pending()) this.dismiss.emit(this.timer().id);
  }

  protected onSnooze(): void {
    if (!this.pending()) this.snooze.emit(this.timer().id);
  }

  protected onCancel(): void {
    if (!this.pending()) this.cancelTimer.emit(this.timer().id);
  }

  /** Ambient work stops when the window is hidden (docs/12 §6). */
  private readonly onVisibility = (): void => this.retick();

  private retick(): void {
    this.stop();
    if (untracked(() => this.remaining()) === null || document.hidden || !this.ticking) return;
    this.handle = setInterval(() => {
      const secs = untracked(() => this.remaining());
      if (secs === null) return;
      if (secs <= 0) {
        // It has reached zero; the server's `timer.fired` event is what flips
        // the state. The card holds at 0:00 rather than counting negative.
        this.stop();
        return;
      }
      this.remaining.set(secs - 1);
    }, TICK_MS);
  }

  private stop(): void {
    if (this.handle !== null) {
      clearInterval(this.handle);
      this.handle = null;
    }
  }
}

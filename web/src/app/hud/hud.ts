import {
  ChangeDetectionStrategy,
  Component,
  DOCUMENT,
  OnDestroy,
  OnInit,
  computed,
  inject,
} from '@angular/core';
import { BUNDLED_WALLPAPERS } from './backgrounds';
import { hudCardId } from './cards/card-id';
import { HudCard } from './cards/hud-card';
import { HudStateService, isApproval } from './hud-state.service';
import { PresenceOrb } from './presence-orb';

/** The "abstract" background when no photo is configured (docs/12 §5). */
const DEFAULT_ABSTRACT_WALLPAPER = BUNDLED_WALLPAPERS[0].asset;

/** How often the silent TTL sweep runs (docs/12 §4). A minute is far finer
 * than the 2-hour TTL and costs nothing; it is paused with the window. */
const SWEEP_INTERVAL_MS = 60_000;

/**
 * The HUD face (docs/12 §1/§2): presence orb, spoken caption, and the
 * materialization canvas. This is the **front face** — there is no chat
 * transcript here; the operator console (Run Spine, timeline, approval detail,
 * diagnostics) is one keystroke away in the ops layer, which the shell owns.
 *
 * F3b.1 shipped the scaffold; F3b.2 fills the canvas with the registered card
 * grammar (`HudStateService.cards`) and its reveal animation; F3b.4 adds the
 * panel lifecycle (shelf row, dismissal, silent TTL sweep) and the background
 * layer. There is still no server-side producer pushing cards onto the wire —
 * F3b.6 is the first.
 */
@Component({
  selector: 'app-hud',
  imports: [PresenceOrb, HudCard],
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './hud.html',
  styleUrl: './hud.scss',
  host: {
    // The glass system adapts as a unit (docs/12 §5): the active background's
    // token set is written once here and inherited by every glass surface
    // below. No component hand-tunes itself for a wallpaper.
    '[style.--glass-alpha]': 'hud.glass().alpha',
    '[style.--glass-blur]': 'hud.glass().blur',
    '[style.--glass-border]': 'hud.glass().border',
    '[style.--glass-shadow]': 'hud.glass().shadow',
    '[style.--ink-dim]': 'hud.glass().inkDim',
  },
})
export class Hud implements OnInit, OnDestroy {
  private readonly document = inject(DOCUMENT);
  protected readonly hud = inject(HudStateService);

  private motionQuery: MediaQueryList | null = null;
  private readonly onVisibility = () => this.syncWindowActive();
  private readonly onMotionChange = (event: MediaQueryListEvent) =>
    this.hud.setReducedMotion(event.matches);

  /** The canvas is announced as empty rather than silently blank (docs/12 §8). */
  protected readonly canvasEmpty = computed(() => this.hud.cards().length === 0);

  /** Stable `@for` key — every card type keys off its own `id` except the
   * wire-reused approval card, which keys off `approvalId` (`card-id.ts`). */
  protected readonly cardKey = hudCardId;

  /** Approvals carry no dismiss affordance — they persist until decided
   * (docs/12 §4). */
  protected readonly isApproval = isApproval;

  /** The wallpaper asset, as a CSS `url()` (FR-23, docs/12 §5). */
  protected readonly wallpaperUrl = computed(() => {
    const asset = this.hud.backgroundAsset() ?? DEFAULT_ABSTRACT_WALLPAPER;
    // Quoted so a path with spaces or parentheses cannot break out of url().
    return `url("${encodeURI(asset)}")`;
  });

  private sweepTimer: ReturnType<typeof setInterval> | null = null;

  ngOnInit(): void {
    const view = this.document.defaultView;
    if (view?.matchMedia) {
      this.motionQuery = view.matchMedia('(prefers-reduced-motion: reduce)');
      this.hud.setReducedMotion(this.motionQuery.matches);
      this.motionQuery.addEventListener('change', this.onMotionChange);
    }
    this.document.addEventListener('visibilitychange', this.onVisibility);
    view?.addEventListener('focus', this.onVisibility);
    view?.addEventListener('blur', this.onVisibility);
    this.syncWindowActive();
    // Silent TTL expiry (docs/12 §4): panels simply stop being there. Swept on
    // a coarse timer rather than per-render so nothing animates on expiry.
    this.sweepTimer = setInterval(() => this.hud.sweepExpired(), SWEEP_INTERVAL_MS);
  }

  ngOnDestroy(): void {
    const view = this.document.defaultView;
    this.motionQuery?.removeEventListener('change', this.onMotionChange);
    this.document.removeEventListener('visibilitychange', this.onVisibility);
    view?.removeEventListener('focus', this.onVisibility);
    view?.removeEventListener('blur', this.onVisibility);
    if (this.sweepTimer !== null) {
      clearInterval(this.sweepTimer);
      this.sweepTimer = null;
    }
    this.hud.stopReveal();
  }

  /** Ambient motion stops when the window is hidden **or** unfocused (docs/12 §6). */
  private syncWindowActive(): void {
    const view = this.document.defaultView;
    const focused = view ? this.document.hasFocus() : true;
    this.hud.setWindowActive(!this.document.hidden && focused);
  }

  protected openOps(): void {
    this.hud.setOpsOpen(true);
  }
}

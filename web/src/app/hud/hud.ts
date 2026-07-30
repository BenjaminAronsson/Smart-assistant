import {
  ChangeDetectionStrategy,
  Component,
  DOCUMENT,
  OnDestroy,
  OnInit,
  computed,
  inject,
} from '@angular/core';
import { hudCardId } from './cards/card-id';
import { HudCard } from './cards/hud-card';
import { HudStateService } from './hud-state.service';
import { PresenceOrb } from './presence-orb';

/**
 * The HUD face (docs/12 §1/§2): presence orb, spoken caption, and the
 * materialization canvas. This is the **front face** — there is no chat
 * transcript here; the operator console (Run Spine, timeline, approval detail,
 * diagnostics) is one keystroke away in the ops layer, which the shell owns.
 *
 * F3b.1 shipped the scaffold; F3b.2 fills the canvas with the registered card
 * grammar (`HudStateService.cards`) and its reveal animation. There is still
 * no server-side producer pushing cards onto the wire (F3b.6 is the first) —
 * the shelf row and panel lifecycle (shelve/restore/dismiss/TTL) are F3b.4.
 */
@Component({
  selector: 'app-hud',
  imports: [PresenceOrb, HudCard],
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './hud.html',
  styleUrl: './hud.scss',
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
  }

  ngOnDestroy(): void {
    const view = this.document.defaultView;
    this.motionQuery?.removeEventListener('change', this.onMotionChange);
    this.document.removeEventListener('visibilitychange', this.onVisibility);
    view?.removeEventListener('focus', this.onVisibility);
    view?.removeEventListener('blur', this.onVisibility);
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

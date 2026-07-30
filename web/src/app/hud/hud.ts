import {
  ChangeDetectionStrategy,
  Component,
  DOCUMENT,
  OnDestroy,
  OnInit,
  computed,
  inject,
} from '@angular/core';
import { HudStateService } from './hud-state.service';
import { PresenceOrb } from './presence-orb';

/**
 * The HUD face (docs/12 §1/§2): presence orb, spoken caption, and the
 * materialization canvas. This is the **front face** — there is no chat
 * transcript here; the operator console (Run Spine, timeline, approval detail,
 * diagnostics) is one keystroke away in the ops layer, which the shell owns.
 *
 * F3b.1 ships the scaffold: the canvas is empty until the card grammar lands in
 * F3b.2 and artifact renderers in F3b.3, and the shelf row is F3b.4's panel
 * lifecycle. What is real here is the state language, the caption, the ops
 * toggle, and the motion/power policy.
 */
@Component({
  selector: 'app-hud',
  imports: [PresenceOrb],
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
  protected readonly canvasEmpty = computed(() => true);

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

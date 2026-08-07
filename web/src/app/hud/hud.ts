import {
  ChangeDetectionStrategy,
  Component,
  DOCUMENT,
  OnDestroy,
  OnInit,
  computed,
  inject,
  signal,
} from '@angular/core';
import { BUNDLED_WALLPAPERS, type BackgroundKind } from './backgrounds';
import { hudCardId } from './cards/card-id';
import { HudCard } from './cards/hud-card';
import { HudStateService, isApproval } from './hud-state.service';
import { PresenceOrb } from './presence-orb';
import { VoicePtt } from '../voice-ptt';
import { ApiService } from '../api.service';
import type {
  ApprovalDecisionDto,
  HudCardDto,
  ProviderState,
  ProvidersResponse,
} from '../../generated/api-types';

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
 * layer. Deep-dive and deterministic list producers now push transient canvas
 * instructions through the authenticated shell stream.
 */
@Component({
  selector: 'app-hud',
  imports: [PresenceOrb, HudCard, VoicePtt],
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
  private readonly api = inject(ApiService);
  private readonly resolvingApprovals = signal<ReadonlySet<string>>(new Set());
  protected readonly providers = signal<ProvidersResponse | null>(null);
  private readonly clock = signal(Date.now());
  protected readonly ambientMotes = Array.from({ length: 18 }, (_, index) => index);

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

  protected isApprovalPending(card: HudCardDto): boolean {
    return isApproval(card) && this.resolvingApprovals().has(card.card.approvalId);
  }

  /** The wallpaper asset, as a CSS `url()` (FR-23, docs/12 §5). */
  protected readonly wallpaperUrl = computed(() => {
    const asset = this.hud.backgroundAsset() ?? DEFAULT_ABSTRACT_WALLPAPER;
    // Quoted so a path with spaces or parentheses cannot break out of url().
    return `url("${encodeURI(asset)}")`;
  });

  private sweepTimer: ReturnType<typeof setInterval> | null = null;
  private providerTimer: ReturnType<typeof setInterval> | null = null;
  private photoObjectUrl: string | null = null;

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
    void this.loadProviders();
    this.providerTimer = setInterval(() => {
      this.clock.set(Date.now());
      void this.loadProviders();
    }, 30_000);
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
    if (this.providerTimer !== null) {
      clearInterval(this.providerTimer);
      this.providerTimer = null;
    }
    if (this.photoObjectUrl !== null) {
      URL.revokeObjectURL(this.photoObjectUrl);
      this.photoObjectUrl = null;
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

  protected getProviderState(): ProviderState | null {
    return this.providers()?.providers[0]?.state ?? null;
  }

  protected getProviderLabel(): string {
    const provider = this.providers()?.providers[0];
    if (!provider) return 'Provider · checking';
    const state = provider.state === 'unavailable' ? 'degraded' : provider.state;
    return `${provider.id} · ${state}`;
  }

  protected getQuotaReset(): string | null {
    return this.providers()?.providers[0]?.quota?.resetAt ?? null;
  }

  protected getQuotaResetLabel(): string | null {
    const resetAt = this.getQuotaReset();
    if (!resetAt) return null;
    const remaining = new Date(resetAt).getTime() - this.clock();
    if (remaining <= 0) return 'now';
    const minutes = Math.ceil(remaining / 60_000);
    if (minutes < 60) return `${minutes}m`;
    const hours = Math.floor(minutes / 60);
    const remainder = minutes % 60;
    return remainder === 0 ? `${hours}h` : `${hours}h ${remainder}m`;
  }

  private async loadProviders(): Promise<void> {
    if (!this.api.hasToken()) return;
    try {
      this.providers.set(await this.api.getProviders());
    } catch {
      // The HUD keeps its last known provider state; the orb carries transport
      // failure and the ops layer contains the detailed diagnostic.
    }
  }

  /** FR-23: the HUD owns the background picker so its glass token swap is atomic. */
  protected onBackgroundChange(event: Event): void {
    const kind = (event.target as HTMLSelectElement).value as BackgroundKind;
    if (kind === 'photo') {
      // The bundled dusk asset is a safe, deterministic photo-mode fallback;
      // a future profile setting can replace it with a user-supplied asset.
      this.hud.setBackground(kind, BUNDLED_WALLPAPERS[1].asset);
    } else {
      this.hud.setBackground(kind);
    }
  }

  protected onPhotoSelected(event: Event): void {
    const file = (event.target as HTMLInputElement).files?.[0];
    if (!file || !file.type.startsWith('image/')) return;
    if (this.photoObjectUrl !== null) URL.revokeObjectURL(this.photoObjectUrl);
    this.photoObjectUrl = URL.createObjectURL(file);
    this.hud.setBackground('photo', this.photoObjectUrl);
  }

  protected async onApprovalDecision(
    card: HudCardDto,
    decision: ApprovalDecisionDto,
  ): Promise<void> {
    if (!isApproval(card) || this.isApprovalPending(card)) return;
    const approval = card.card;
    this.resolvingApprovals.update((ids) => new Set(ids).add(approval.approvalId));
    try {
      await this.api.resolveApproval(approval.runId, approval.approvalId, decision);
    } catch {
      this.resolvingApprovals.update((ids) => {
        const next = new Set(ids);
        next.delete(approval.approvalId);
        return next;
      });
      this.hud.speak('I could not send that decision. Please try again.');
      this.hud.setPresence('error');
    }
  }
}

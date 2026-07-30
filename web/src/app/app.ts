import { KeyValuePipe } from '@angular/common';
import {
  Component,
  HostListener,
  OnDestroy,
  OnInit,
  inject,
  signal,
  effect,
  ChangeDetectionStrategy,
} from '@angular/core';
import { takeUntilDestroyed } from '@angular/core/rxjs-interop';
import { NavigationEnd, Router, RouterLink, RouterOutlet } from '@angular/router';
import type { HealthResponse, SessionDto } from '../generated/api-types';
import { ApiService } from './api.service';
import { Hud } from './hud/hud';
import { HudStateService } from './hud/hud-state.service';
import { MediaBar } from './media-bar';
import { MediaService } from './media.service';

/**
 * Jarvis shell root (docs/03 §3, docs/12 §1).
 *
 * **The front face is the HUD** (F3b.1): presence orb, caption, materialization
 * canvas. The M0/M1 operator surfaces — health, pairing, session list and the
 * conversation/timeline/approval views — are not deleted; they move behind the
 * **ops layer**, one keystroke away (`Ctrl+.` or clicking the orb), exactly as
 * docs/12 §1 describes. Approvals are the documented exception and interrupt
 * onto the HUD face itself once cards land (F3b.2).
 *
 * The media bar (F3a.7, FR-22) stays at the shell root because it is ambient: it
 * controls whatever is playing regardless of layer, and is absent when nothing is.
 */
@Component({
  selector: 'app-root',
  imports: [KeyValuePipe, RouterLink, RouterOutlet, MediaBar, Hud],
  templateUrl: './app.html',
  changeDetection: ChangeDetectionStrategy.OnPush,
  styleUrl: './app.scss',
})
export class App implements OnInit, OnDestroy {
  private readonly api = inject(ApiService);
  protected readonly media = inject(MediaService);
  protected readonly hud = inject(HudStateService);
  private readonly router = inject(Router);

  protected readonly title = signal('Jarvis');
  protected readonly health = signal<HealthResponse | null>(null);
  protected readonly sessions = signal<SessionDto[]>([]);
  protected readonly paired = signal(false);
  protected readonly error = signal<string | null>(null);
  protected readonly newSessionTitle = signal('');

  /**
   * Which layer the active route belongs to (`data.surface`, see `app.routes`).
   * `null` when no route is active — the bare HUD face.
   */
  protected readonly routedSurface = signal<'hud' | 'ops' | null>(null);

  constructor() {
    // Track the deepest activated route's surface so the outlet renders in the
    // right layer. An ops-surface route also opens the layer: navigating to a
    // conversation should show it, not leave it hidden behind Ctrl+.
    this.router.events.pipe(takeUntilDestroyed()).subscribe((event) => {
      if (!(event instanceof NavigationEnd)) {
        return;
      }
      let route = this.router.routerState.root;
      while (route.firstChild) {
        route = route.firstChild;
      }
      const surface = route.snapshot.data['surface'];
      const resolved = surface === 'hud' ? 'hud' : surface === 'ops' ? 'ops' : null;
      this.routedSurface.set(resolved);
      if (resolved === 'ops') {
        this.hud.setOpsOpen(true);
      }
    });

    // Presence is derived, never set by hand: an unreachable or degraded daemon
    // is a HUD state (docs/12 §2.1), not just a line of text in the ops layer.
    // Run-driven states (listening/speaking/tool/waiting) arrive with the HUD's
    // run stream in F3b.2/F3b.6 — this is the part that exists today.
    effect(() => {
      const health = this.health();
      if (this.error() !== null || health === null) {
        this.hud.setPresence('error');
      } else if (health.status !== 'ok') {
        this.hud.setPresence('degraded');
      } else {
        this.hud.setPresence('idle');
      }
    });
  }

  ngOnInit(): void {
    this.paired.set(this.api.hasToken());
    void this.refresh();
    // Media control is device-authenticated: only start it once paired.
    if (this.paired()) {
      void this.media.start();
    }
  }

  ngOnDestroy(): void {
    this.media.stop();
  }

  /**
   * `Ctrl+.` toggles the ops layer (docs/12 §1/§8); `Escape` closes it, so the
   * HUD face is always one key from the front. Both are documented keyboard
   * paths — the orb click is the pointer equivalent, never the only way in.
   */
  @HostListener('document:keydown', ['$event'])
  protected onKeydown(event: KeyboardEvent): void {
    if (event.ctrlKey && event.key === '.') {
      event.preventDefault();
      this.hud.toggleOps();
    } else if (event.key === 'Escape' && this.hud.opsOpen()) {
      this.hud.setOpsOpen(false);
    }
  }

  protected onMediaCommand(event: { command: string; player: string }): void {
    void this.media.command(event.command, event.player);
  }

  protected onMediaVolume(event: { player: string; volumePct: number }): void {
    void this.media.setVolume(event.player, event.volumePct);
  }

  protected async refresh(): Promise<void> {
    try {
      this.health.set(await this.api.health());
      this.error.set(null);
      if (this.paired()) {
        this.sessions.set((await this.api.listSessions()).sessions);
      }
    } catch {
      this.error.set('jarvisd is not reachable');
    }
  }

  protected async pair(): Promise<void> {
    const code = this.health()?.pairingCode;
    if (!code) {
      return;
    }
    try {
      await this.api.pair(code, 'web-shell');
      this.paired.set(true);
      await this.refresh();
      // Now authenticated: the media surface becomes reachable.
      void this.media.start();
    } catch {
      this.error.set('pairing failed');
    }
  }

  protected async createSession(): Promise<void> {
    const title = this.newSessionTitle().trim();
    try {
      await this.api.createSession(title === '' ? undefined : title, crypto.randomUUID());
      this.newSessionTitle.set('');
      await this.refresh();
    } catch {
      this.error.set('session create failed');
    }
  }

  protected onTitleInput(event: Event): void {
    this.newSessionTitle.set((event.target as HTMLInputElement).value);
  }
}

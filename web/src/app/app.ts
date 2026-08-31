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
import type {
  DomainEvent,
  EventEnvelope,
  HealthResponse,
  MessageDto,
  RunStateDto,
  SessionDto,
  TransientEvent,
} from '../generated/api-types';
import { ApiService } from './api.service';
import { Hud } from './hud/hud';
import { HudStateService, presenceForRunState } from './hud/hud-state.service';
import { MediaBar } from './media-bar';
import { MediaService } from './media.service';

const TRANSIENT_WS_TYPES = new Set(['text.delta', 'media.state', 'hud.canvas', 'degraded.queued']);

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
  private hudWs: WebSocket | null = null;
  private hudReconnect: ReturnType<typeof setTimeout> | null = null;
  private hudLastSeq: number | null = null;

  protected readonly title = signal('Jarvis');
  protected readonly health = signal<HealthResponse | null>(null);
  protected readonly sessions = signal<SessionDto[]>([]);
  protected readonly paired = signal(false);
  protected readonly error = signal<string | null>(null);
  protected readonly newSessionTitle = signal('');
  private readonly liveRun = signal(false);

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
    // Voice capture owns the listening state; run/approval events own
    // speaking/tool/waiting. All paths converge on the same HUD state service.
    effect(() => {
      const health = this.health();
      if (this.error() !== null || health === null) {
        this.hud.setPresence('error');
      } else if (this.liveRun()) {
        // A healthy daemon does not mean an active run is idle. Run events own
        // the HUD state until the terminal event arrives.
        return;
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
      this.connectHudStream();
    }
  }

  ngOnDestroy(): void {
    this.media.stop();
    if (this.hudReconnect !== null) clearTimeout(this.hudReconnect);
    this.hudWs?.close();
    this.hudWs = null;
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
      this.hud.applyConfiguredUi(this.health()?.ui);
      this.error.set(null);
      if (this.paired()) {
        try {
          this.sessions.set((await this.api.listSessions()).sessions);
        } catch {
          // Health is still authoritative: a degraded database should keep
          // the HUD gray/degraded rather than turning the whole face red.
        }
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
      this.connectHudStream();
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

  /**
   * The HUD is the default route, so it must receive global run/canvas events
   * even when the operator conversation component is not mounted. The same
   * authenticated stream also makes approval interrupts real rather than
   * route-dependent.
   */
  private connectHudStream(resume = false): void {
    if (!this.paired() || this.hudWs !== null) return;
    const since = resume && this.hudLastSeq !== null ? `?since=${this.hudLastSeq}` : '';
    this.hudWs = this.api.openSocket(`/ws/v1${since}`);
    this.hudWs.onmessage = (message) => {
      try {
        this.handleHudEnvelope(JSON.parse(message.data) as EventEnvelope);
      } catch {
        // The operator route owns detailed parse diagnostics; the HUD stream
        // remains fail-closed when an event is malformed.
      }
    };
    this.hudWs.onclose = () => {
      this.hudWs = null;
      if (this.paired()) {
        this.hudReconnect = setTimeout(() => {
          this.hudReconnect = null;
          this.connectHudStream(true);
        }, 3000);
      }
    };
  }

  private handleHudEnvelope(env: EventEnvelope): void {
    if (env.channel !== 'session') return;

    // Transient events reuse the durable high-water sequence and therefore do
    // not advance the `since` cursor or participate in gap detection.
    if (!TRANSIENT_WS_TYPES.has(env.type)) {
      if (this.hudLastSeq !== null && env.seq !== this.hudLastSeq + 1) {
        const runId = (env.payload as { runId?: string }).runId;
        if (runId) void this.resyncHudRun(runId);
      }
      this.hudLastSeq = env.seq;
    }

    if (env.type === 'hud.canvas') {
      const transient = env.payload as TransientEvent;
      if (transient.type === 'hud.canvas') {
        this.hud.routeTurn(
          transient.canvas.action,
          transient.canvas.label,
          transient.canvas.cards ?? [],
        );
      }
      return;
    }

    if (env.type === 'degraded.queued') {
      const queued = env.payload as Extract<TransientEvent, { type: 'degraded.queued' }>;
      this.hud.markRunQueued(queued.runId);
      this.liveRun.set(true);
      this.hud.setPresence('degraded');
      return;
    }

    const event = { ...(env.payload as Record<string, unknown>), type: env.type } as DomainEvent;
    switch (event.type) {
      case 'run.queued':
        this.liveRun.set(true);
        this.hud.markRunQueued(event.runId);
        this.hud.setPresence('degraded');
        break;
      case 'run.started':
        this.liveRun.set(true);
        this.hud.clearQueuedRun(event.runId);
        this.hud.setPresence('speaking');
        break;
      case 'run.state_changed':
        this.liveRun.set(!['completed', 'failed', 'cancelled'].includes(event.state));
        if (['completed', 'failed', 'cancelled'].includes(event.state)) {
          this.hud.clearQueuedRun(event.runId);
        }
        this.setHudPresenceForRunState(event.state);
        break;
      case 'run.completed':
        this.liveRun.set(false);
        this.hud.clearQueuedRun(event.runId);
        this.hud.setPresence(event.outcome.kind === 'completed' ? 'done' : 'error');
        break;
      case 'provider.health_changed':
        this.hud.setPresence(event.provider.state === 'healthy' ? 'idle' : 'degraded');
        break;
      case 'approval.requested':
        this.hud.appendCards([{ type: 'card.approval', card: event.card }]);
        this.hud.setPresence('waiting');
        break;
      case 'approval.resolved':
        this.hud.resolveApproval(event.approvalId);
        break;
      case 'message.created':
        this.speakMessage(event.message);
        break;
      case 'run.checkpoint_saved':
      case 'timer.fired':
        break;
    }
  }

  /** Repair the global HUD's live presence from the durable run snapshot. */
  private async resyncHudRun(runId: string): Promise<void> {
    try {
      const run = await this.api.getRun(runId);
      this.liveRun.set(!['completed', 'failed', 'cancelled'].includes(run.state));
      this.setHudPresenceForRunState(run.state);
    } catch {
      // Conversation owns the session timeline repair; a HUD snapshot failure
      // leaves the last visible evidence intact and the next event converges it.
    }
  }

  private setHudPresenceForRunState(state: RunStateDto): void {
    this.hud.setPresence(presenceForRunState(state));
  }

  private speakMessage(message: MessageDto): void {
    const text = message.content
      .map((block) => (block.type === 'text' ? block.text : ''))
      .join('')
      .trim();
    if (text) this.hud.speak(text);
  }
}

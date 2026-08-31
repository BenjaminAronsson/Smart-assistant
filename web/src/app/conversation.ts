import {
  Component,
  HostListener,
  OnInit,
  OnDestroy,
  inject,
  signal,
  ChangeDetectionStrategy,
} from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';
import { ActivatedRoute } from '@angular/router';
import type {
  SessionDto,
  DomainEvent,
  EventEnvelope,
  HudCanvasDto,
  TimelineItem,
  MessageDto,
  ProviderState,
  ProvidersResponse,
  RunStateDto,
  ApprovalCardDto,
  ApprovalDecisionDto,
  TransientEvent,
} from '../generated/api-types';
import { ApiService } from './api.service';
import { ApprovalTray } from './approval-tray';
import { HudStateService, presenceForRunState } from './hud/hud-state.service';
import { VoiceCaptureService } from './voice-capture.service';

const TRANSIENT_WS_TYPES = new Set(['text.delta', 'media.state', 'hud.canvas', 'degraded.queued']);

/** Cap on the live streaming preview buffer (NIT 4). The durable message that
 * arrives on completion is authoritative, so trimming the transient preview to
 * the most recent characters is safe. Generous vs. any real single response. */
const MAX_STREAMING_CHARS = 100_000;

/**
 * Conversation/timeline view (F1.8): displays messages and run events,
 * including queued/waiting runs to show degraded mode (visible queueing).
 * Real-time updates via WS; timeline resync on reconnect.
 */
@Component({
  selector: 'app-conversation',
  standalone: true,
  imports: [CommonModule, FormsModule, ApprovalTray],
  templateUrl: './conversation.html',
  changeDetection: ChangeDetectionStrategy.OnPush,
  styleUrl: './conversation.scss',
})
export class Conversation implements OnInit, OnDestroy {
  private readonly api = inject(ApiService);
  private readonly route = inject(ActivatedRoute);
  /** The materialization canvas (docs/12 §2.3). The deep-dive router's
   * `hud.canvas` instruction is applied to it below (F3b.6). */
  protected readonly hud = inject(HudStateService);
  /** Live partial/final recognition text; the durable user message remains the
   * source of truth once the voice turn is submitted. */
  protected readonly voice = inject(VoiceCaptureService);

  protected readonly session = signal<SessionDto | null>(null);
  protected readonly timeline = signal<TimelineItem[]>([]);
  protected readonly providers = signal<ProvidersResponse | null>(null);
  protected readonly messageText = signal('');
  protected readonly loading = signal(false);
  protected readonly error = signal<string | null>(null);
  /** Live-accumulated token deltas for the in-progress response (transient,
   * never persisted — docs/05 §3). Cleared when the durable message arrives. */
  protected readonly streamingText = signal('');
  /** Pending R2/R3 approvals interrupting the surface (docs/12 §2.3). Populated
   * from live `approval.requested`/`approval.resolved` and reconciled from the
   * timeline snapshot on reconnect — approvals persist until decided (docs/12
   * §4: exempt from TTL), so a reconnect must restore them. */
  protected readonly pendingApprovals = signal<ApprovalCardDto[]>([]);
  /** Approval ids whose decision is in flight — optimistic-block until the
   * durable `approval.resolved` event removes the card (angular-shell §4). */
  protected readonly resolving = signal<ReadonlySet<string>>(new Set());
  /** Run cancellation remains available while model or tool work is active. */
  protected readonly activeRunId = signal<string | null>(null);
  protected readonly activeRunState = signal<RunStateDto | null>(null);
  protected readonly cancelling = signal(false);
  private readonly providerNow = signal(Date.now());

  private sessionId: string | null = null;
  private ws: WebSocket | null = null;
  private resyncCursor = 0;
  private lastSeq: number | null = null;
  private destroyed = false;
  private providerInterval: ReturnType<typeof setInterval> | null = null;

  ngOnInit(): void {
    this.sessionId = this.route.snapshot.paramMap.get('id');
    if (!this.sessionId) {
      this.error.set('Session ID not found');
      return;
    }

    void this.loadSession();
    void this.loadTimeline();
    void this.loadProviders();
    this.connectWebSocket();

    // Refresh providers periodically (F1.7: health polling); cleared on
    // destroy below. (`effect()` requires an injection context, which a
    // plain lifecycle method is not — a bare `setInterval`/`ngOnDestroy`
    // pair, matching the WS teardown just below, needs none.)
    this.providerInterval = setInterval(() => {
      this.providerNow.set(Date.now());
      void this.loadProviders();
    }, 10000);
  }

  ngOnDestroy(): void {
    this.destroyed = true;
    if (this.ws) {
      this.ws.close();
    }
    if (this.providerInterval !== null) {
      clearInterval(this.providerInterval);
    }
  }

  private async loadSession(): Promise<void> {
    if (!this.sessionId) return;
    try {
      const resp = await this.api.getSession(this.sessionId);
      this.session.set(resp);
    } catch {
      this.error.set('Failed to load session');
    }
  }

  private async loadTimeline(): Promise<void> {
    if (!this.sessionId) return;
    try {
      const resp = await this.api.getTimeline(this.sessionId, this.resyncCursor);
      // Approvals persist until decided (docs/12 §4), so rebuild the pending set
      // from the snapshot before rendering — a reconnect must not drop a card.
      this.reconcilePendingFromTimeline(resp.items);
      // Approval events drive the interrupt tray, not the scrolling history, so
      // keep them out of the displayed timeline (they render as cards instead).
      this.timeline.set(resp.items.filter((item) => !this.isApprovalEvent(item)));
      if (resp.nextSince !== null && resp.nextSince !== undefined) {
        this.resyncCursor = resp.nextSince;
      }
    } catch {
      this.error.set('Failed to load timeline');
    }
  }

  private async loadProviders(): Promise<void> {
    try {
      const resp = await this.api.getProviders();
      this.providers.set(resp);
    } catch (err) {
      // Non-fatal: provider info is advisory
      console.warn('Failed to load providers', err);
    }
  }

  private connectWebSocket(): void {
    if (!this.sessionId || this.destroyed) return;

    this.lastSeq = null;
    this.ws = this.api.openSocket('/ws/v1');

    this.ws.onmessage = (event) => {
      try {
        const envelope: EventEnvelope = JSON.parse(event.data);
        if (envelope.channel === 'session') {
          if (!TRANSIENT_WS_TYPES.has(envelope.type) && this.lastSeq !== null && envelope.seq !== this.lastSeq + 1) {
            // Durable timeline truth repairs a dropped WS event; transient
            // token deltas are intentionally not replayed.
            void this.loadTimeline();
          }
          if (!TRANSIENT_WS_TYPES.has(envelope.type)) {
            this.lastSeq = envelope.seq;
          }
        }
        this.handleWebSocketMessage(envelope);
      } catch (err) {
        console.error('Failed to parse WS message', err);
      }
    };

    this.ws.onerror = () => {
      this.error.set('WebSocket connection error');
    };

    this.ws.onclose = () => {
      this.lastSeq = null;
      if (!this.destroyed) {
        // Reconcile before resuming so approvals and run states converge.
        void this.loadTimeline();
        setTimeout(() => this.connectWebSocket(), 3000);
      }
    };
  }

  /**
   * Handle one WS envelope (docs/05 §3). The wire shape is
   * `{ channel, type, payload, seq, … }`: the event discriminator lives on the
   * envelope's `type`, and the payload carries only the event's own fields (the
   * server splits the tag out, see jarvisd `ws::split_tagged`). We fold `type`
   * back into the payload to reconstruct the generated `DomainEvent` union —
   * the same decode the timeline resync performs.
   */
  private handleWebSocketMessage(env: EventEnvelope): void {
    if (env.channel !== 'session') return;

    // Transient token delta: accumulate into the live response bubble. A durable
    // `message.created` follows on completion and replaces it. Capped so a
    // runaway/hostile stream can't grow the buffer without bound (NIT 4); the
    // durable message is the source of truth, so a trimmed live preview is fine.
    if (env.type === 'text.delta') {
      const delta = (env.payload as { text?: string }).text ?? '';
      this.streamingText.update((prev) => {
        const next = prev + delta;
        return next.length > MAX_STREAMING_CHARS ? next.slice(-MAX_STREAMING_CHARS) : next;
      });
      return;
    }

    if (env.type === 'degraded.queued') {
      const queued = env.payload as Extract<TransientEvent, { type: 'degraded.queued' }>;
      if (queued.runId === this.activeRunId() || this.activeRunId() === null) {
        this.hud.markRunQueued(queued.runId);
        this.activeRunId.set(queued.runId);
        this.activeRunState.set('received');
        this.hud.setPresence('degraded');
      }
      return;
    }

    // Transient canvas instruction (F3b.6, FR-27/ADR-017): the server decided
    // continuation-vs-new-topic and says what this turn does to the canvas.
    // The decision is never re-derived here — there is one classifier, and it
    // is the one the human can correct by voice ("new topic").
    if (env.type === 'hud.canvas') {
      const canvas = (env.payload as { canvas?: HudCanvasDto }).canvas;
      if (canvas && this.canvasIsForThisView(canvas)) {
        this.hud.routeTurn(canvas.action, canvas.label, canvas.cards ?? []);
      }
      return;
    }

    const event = {
      ...(env.payload as Record<string, unknown>),
      type: env.type,
    } as DomainEvent;

    switch (event.type) {
      case 'message.created': {
        // The durable message supersedes any in-progress streamed text.
        this.streamingText.set('');
        const captionText = this.getMessageText(event.message).trim();
        if (captionText) this.hud.speak(captionText);
        this.timeline.update((items) => [...items, { type: 'message', message: event.message }]);
        break;
      }
      case 'run.queued':
        this.activeRunId.set(event.runId);
        this.activeRunState.set('received');
        this.hud.markRunQueued(event.runId);
        this.hud.setPresence('degraded');
        this.timeline.update((items) => [...items, { type: 'run_event', event }]);
        break;
      case 'run.started':
        this.activeRunId.set(event.runId);
        this.activeRunState.set('model_running');
        this.hud.clearQueuedRun(event.runId);
        this.hud.setPresence('speaking');
        this.timeline.update((items) => [...items, { type: 'run_event', event }]);
        break;
      case 'run.state_changed':
        this.activeRunId.set(event.runId);
        this.activeRunState.set(event.state);
        if (['completed', 'failed', 'cancelled'].includes(event.state)) {
          this.hud.clearQueuedRun(event.runId);
        }
        this.setHudPresenceForRunState(event.state);
        this.timeline.update((items) => [...items, { type: 'run_event', event }]);
        break;
      case 'run.completed':
        this.hud.clearQueuedRun(event.runId);
        if (this.activeRunId() === event.runId) {
          this.activeRunId.set(null);
          this.activeRunState.set(null);
          this.cancelling.set(false);
        }
        this.hud.setPresence(event.outcome.kind === 'completed' ? 'done' : 'error');
        this.streamingText.set('');
        this.timeline.update((items) => [...items, { type: 'run_event', event }]);
        break;
      case 'provider.health_changed':
        this.hud.setPresence(event.provider.state === 'healthy' ? 'idle' : 'degraded');
        void this.loadProviders();
        break;
      case 'approval.requested':
        this.addPendingApproval(event.card);
        // Approval interrupts belong on the HUD face as well as the ops tray.
        this.hud.appendCards([{ type: 'card.approval', card: event.card }]);
        break;
      case 'approval.resolved':
        // The durable decision is the source of truth — drop the card and clear
        // any optimistic block, whether this client or another decided it.
        this.removePendingApproval(event.approvalId);
        this.hud.resolveApproval(event.approvalId);
        break;
    }
  }

  /**
   * Whether a canvas instruction belongs to the conversation this view is
   * showing (F3b.6).
   *
   * The WS fan-out is **global** — one broadcast reaches every authenticated
   * device and every open conversation — so `sessionId` in the payload is the
   * only thing scoping a canvas instruction to a conversation. Ignoring it let
   * session B's sources and gallery cards render on session A's canvas, and a
   * `shelve` from B shelve A's panels.
   *
   * A `null`/absent `sessionId` is *not* an unscoped leak: it is the documented
   * "applies anywhere" case (a list card from the deterministic list grammar,
   * FR-34, which has no session at all), so it must pass. Only a canvas that
   * names a *different* conversation is dropped.
   */
  private canvasIsForThisView(canvas: HudCanvasDto): boolean {
    return !canvas.sessionId || canvas.sessionId === this.sessionId;
  }

  /** A human decided an approval; block the card and POST the decision. The card
   * is removed only when the durable `approval.resolved` event confirms it. */
  protected async onDecide(card: ApprovalCardDto, decision: ApprovalDecisionDto): Promise<void> {
    if (this.resolving().has(card.approvalId)) return;
    this.resolving.update((ids) => new Set(ids).add(card.approvalId));
    try {
      await this.api.resolveApproval(card.runId, card.approvalId, decision);
    } catch {
      // The decision did not land — unblock so the human can retry.
      this.resolving.update((ids) => {
        const next = new Set(ids);
        next.delete(card.approvalId);
        return next;
      });
      this.error.set('Failed to send decision');
    }
  }

  protected isResolving(approvalId: string): boolean {
    return this.resolving().has(approvalId);
  }

  protected trackByApprovalId(_index: number, card: ApprovalCardDto): string {
    return card.approvalId;
  }

  private isApprovalEvent(item: TimelineItem): boolean {
    return (
      item.type === 'run_event' &&
      (item.event.type === 'approval.requested' || item.event.type === 'approval.resolved')
    );
  }

  private addPendingApproval(card: ApprovalCardDto): void {
    this.pendingApprovals.update((cards) =>
      cards.some((existing) => existing.approvalId === card.approvalId) ? cards : [...cards, card],
    );
  }

  private removePendingApproval(approvalId: string): void {
    this.pendingApprovals.update((cards) => cards.filter((c) => c.approvalId !== approvalId));
    this.resolving.update((ids) => {
      if (!ids.has(approvalId)) return ids;
      const next = new Set(ids);
      next.delete(approvalId);
      return next;
    });
  }

  /** Fold a timeline snapshot's approval events into the pending set: a
   * `requested` with no later `resolved` is still awaiting the human. */
  private reconcilePendingFromTimeline(items: TimelineItem[]): void {
    for (const item of items) {
      if (item.type !== 'run_event') continue;
      const event = item.event;
      if (event.type === 'approval.requested') {
        this.addPendingApproval(event.card);
        this.hud.appendCards([{ type: 'card.approval', card: event.card }]);
      } else if (event.type === 'approval.resolved') {
        this.removePendingApproval(event.approvalId);
        this.hud.resolveApproval(event.approvalId);
      }
    }
  }

  protected async submitMessage(): Promise<void> {
    const text = this.messageText().trim();
    if (!text || !this.sessionId) return;

    this.loading.set(true);
    try {
      await this.api.submitMessage(this.sessionId, text);
      this.messageText.set('');
    } catch {
      this.error.set('Failed to submit message');
    } finally {
      this.loading.set(false);
    }
  }

  @HostListener('document:keydown', ['$event'])
  protected onKeydown(event: KeyboardEvent): void {
    if (event.key === 'Escape' && this.canCancelRun() && !this.cancelling()) {
      event.preventDefault();
      void this.cancelActiveRun();
    }
  }

  protected canCancelRun(): boolean {
    return this.activeRunId() !== null &&
      (this.activeRunState() === 'model_running' || this.activeRunState() === 'tool_running');
  }

  protected async cancelActiveRun(): Promise<void> {
    const runId = this.activeRunId();
    if (!runId || !this.canCancelRun() || this.cancelling()) return;

    this.cancelling.set(true);
    try {
      await this.api.cancelRun(runId);
    } catch {
      this.cancelling.set(false);
      this.error.set('Failed to cancel run');
    }
  }

  protected getProviderState(): ProviderState | null {
    const providers = this.providers();
    if (!providers || providers.providers.length === 0) {
      return null;
    }
    return providers.providers[0].state;
  }

  protected getProviderReason(): string | null {
    const providers = this.providers();
    if (!providers || providers.providers.length === 0) {
      return null;
    }
    return providers.providers[0].reason || null;
  }

  protected getProviderLabel(): string {
    const provider = this.providers()?.providers[0];
    if (!provider) return 'Provider · checking';
    const state = provider.state === 'unavailable' ? 'degraded' : provider.state;
    return `${provider.id} · ${state}`;
  }

  private setHudPresenceForRunState(state: RunStateDto): void {
    this.hud.setPresence(presenceForRunState(state));
  }

  protected getQuotaReset(): string | null {
    return this.providers()?.providers[0]?.quota?.resetAt ?? null;
  }

  protected getQuotaResetLabel(): string | null {
    const resetAt = this.getQuotaReset();
    if (!resetAt) return null;
    const remaining = new Date(resetAt).getTime() - this.providerNow();
    if (remaining <= 0) return 'now';
    const minutes = Math.ceil(remaining / 60_000);
    if (minutes < 60) return `${minutes}m`;
    const hours = Math.floor(minutes / 60);
    const remainder = minutes % 60;
    return remainder === 0 ? `${hours}h` : `${hours}h ${remainder}m`;
  }

  protected isProviderUnavailable(): boolean {
    return this.getProviderState() === 'unavailable';
  }

  protected getRunState(event: DomainEvent): RunStateDto | null {
    // Narrows the union to the run.state_changed variant, which carries `state`.
    return event.type === 'run.state_changed' ? event.state : null;
  }

  protected getMessageRole(msg: MessageDto): string {
    return msg.role === 'assistant' ? 'Jarvis' : 'You';
  }

  protected getMessageText(msg: MessageDto): string {
    return msg.content
      .map((block) => (block.type === 'text' ? block.text : ''))
      .join('');
  }

  protected trackByIndex(index: number): number {
    return index;
  }
}

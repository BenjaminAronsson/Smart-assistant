import {
  Component,
  OnInit,
  OnDestroy,
  inject,
  signal,
  ChangeDetectionStrategy,
  effect,
} from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';
import { ActivatedRoute } from '@angular/router';
import type {
  SessionDto,
  DomainEvent,
  TimelineItem,
  TimelineResponse,
  MessageDto,
  ProviderState,
  ProvidersResponse,
  RunStateDto,
} from '../generated/api-types';
import { ApiService } from './api.service';

/**
 * Conversation/timeline view (F1.8): displays messages and run events,
 * including queued/waiting runs to show degraded mode (visible queueing).
 * Real-time updates via WS; timeline resync on reconnect.
 */
@Component({
  selector: 'app-conversation',
  standalone: true,
  imports: [CommonModule, FormsModule],
  templateUrl: './conversation.html',
  changeDetection: ChangeDetectionStrategy.OnPush,
  styleUrl: './conversation.scss',
})
export class Conversation implements OnInit, OnDestroy {
  private readonly api = inject(ApiService);
  private readonly route = inject(ActivatedRoute);

  protected readonly session = signal<SessionDto | null>(null);
  protected readonly timeline = signal<TimelineItem[]>([]);
  protected readonly providers = signal<ProvidersResponse | null>(null);
  protected readonly messageText = signal('');
  protected readonly loading = signal(false);
  protected readonly error = signal<string | null>(null);

  private sessionId: string | null = null;
  private ws: WebSocket | null = null;
  private resyncCursor = 0;

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

    // Refresh providers periodically (F1.7: health polling)
    const providerInterval = setInterval(() => {
      void this.loadProviders();
    }, 10000);

    // Cleanup on destroy
    effect(() => {
      return () => clearInterval(providerInterval);
    });
  }

  ngOnDestroy(): void {
    if (this.ws) {
      this.ws.close();
    }
  }

  private async loadSession(): Promise<void> {
    if (!this.sessionId) return;
    try {
      const resp = await this.api.getSession(this.sessionId);
      this.session.set(resp);
    } catch (err) {
      this.error.set('Failed to load session');
    }
  }

  private async loadTimeline(): Promise<void> {
    if (!this.sessionId) return;
    try {
      const resp = await this.api.getTimeline(this.sessionId, this.resyncCursor);
      this.timeline.set(resp.items);
      if (resp.nextSince !== null && resp.nextSince !== undefined) {
        this.resyncCursor = resp.nextSince;
      }
    } catch (err) {
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
    if (!this.sessionId) return;

    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const wsUrl = `${protocol}//${window.location.host}/ws/v1`;
    this.ws = new WebSocket(wsUrl);

    this.ws.onmessage = (event) => {
      try {
        const msg = JSON.parse(event.data);
        this.handleWebSocketMessage(msg);
      } catch (err) {
        console.error('Failed to parse WS message', err);
      }
    };

    this.ws.onerror = () => {
      this.error.set('WebSocket connection error');
    };

    this.ws.onclose = () => {
      // Attempt to reconnect after a delay
      setTimeout(() => this.connectWebSocket(), 3000);
    };
  }

  private handleWebSocketMessage(msg: any): void {
    // Message format: { seq, type, channel, payload }
    // For session channel: payload contains DomainEvent
    if (msg.channel === 'session' && msg.type === 'message') {
      const event = msg.payload as DomainEvent;

      // Add to timeline
      const currentTimeline = this.timeline();
      let newItem: TimelineItem | null = null;

      if (event.type === 'message.created') {
        newItem = {
          type: 'message',
          message: (event as any).message as MessageDto,
        };
      } else if (
        event.type === 'run.queued' ||
        event.type === 'run.state_changed' ||
        event.type === 'run.completed' ||
        event.type === 'run.started'
      ) {
        newItem = { type: 'run_event', event };
      }

      if (newItem) {
        this.timeline.set([...currentTimeline, newItem]);
      }

      // Refresh providers on health change or run completion
      if (event.type === 'provider.health_changed') {
        void this.loadProviders();
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
    } catch (err) {
      this.error.set('Failed to submit message');
    } finally {
      this.loading.set(false);
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

  protected isProviderUnavailable(): boolean {
    return this.getProviderState() === 'unavailable';
  }

  protected getRunState(event: DomainEvent): RunStateDto | null {
    if (event.type === 'run.state_changed') {
      return (event as any).state as RunStateDto;
    }
    return null;
  }

  protected getMessageRole(msg: MessageDto): string {
    return msg.role === 'assistant' ? 'Jarvis' : 'You';
  }

  protected getMessageText(msg: MessageDto): string {
    return msg.content
      .filter((block) => block.type === 'text')
      .map((block) => (block as any).text || '')
      .join('');
  }

  protected trackByIndex(index: number): number {
    return index;
  }
}

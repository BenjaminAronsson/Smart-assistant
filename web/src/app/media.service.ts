import { Injectable, inject, signal } from '@angular/core';
import type { EventEnvelope, MediaStateDto } from '../generated/api-types';
import { ApiService } from './api.service';

/**
 * Owns the media bar's state (FR-22, docs/02 §11a).
 *
 * The two halves of the contract, both forced by `media.state` being a
 * **transient** event (docs/05 §3):
 *
 * 1. **Read once on connect.** Transient events are never replayed, so a client
 *    that just loaded has no state until something changes. `GET
 *    /api/v1/media/state` supplies the starting value.
 * 2. **Follow events after that.** No polling — the server pushes only on real
 *    changes (it suppresses identical snapshots), so an idle desktop produces no
 *    traffic at all.
 *
 * The socket is opened **only when the server reports media control available**,
 * so a host with no session bus pays nothing. (The shell converges on a single
 * shared socket when the HUD lands in F3b.1; today the conversation view owns
 * its own, and this one exists only while media is actually present.)
 */
@Injectable({ providedIn: 'root' })
export class MediaService {
  private readonly api = inject(ApiService);
  private ws: WebSocket | null = null;
  private reconnect: ReturnType<typeof setTimeout> | null = null;
  private closed = false;

  /** Current playback state; `null` until the first read succeeds. */
  readonly state = signal<MediaStateDto | null>(null);
  /** False when media control is not configured or no session bus exists. */
  readonly available = signal(false);
  /** A command is in flight. */
  readonly pending = signal(false);
  /** Last user-visible failure (e.g. an ambiguous target), or null. */
  readonly error = signal<string | null>(null);

  /** Load the current state and, if media exists, start following changes. */
  async start(): Promise<void> {
    try {
      const response = await this.api.getMediaState();
      this.available.set(response.available);
      this.state.set(response.state);
      if (response.available) {
        this.connect();
      }
    } catch {
      // An unreachable or unwired media surface is not an error the human
      // needs to see: the bar simply does not appear.
      this.available.set(false);
      this.state.set(null);
    }
  }

  /** Stop following changes (component teardown / sign-out). */
  stop(): void {
    this.closed = true;
    if (this.reconnect !== null) {
      clearTimeout(this.reconnect);
      this.reconnect = null;
    }
    this.ws?.close();
    this.ws = null;
  }

  /** Apply a transport verb to a named player. */
  async command(command: string, player: string): Promise<void> {
    await this.send({ command, player });
  }

  /** Set a player's volume. The server enforces the cap regardless of the UI. */
  async setVolume(player: string, volumePct: number): Promise<void> {
    await this.send({ command: 'set_volume', player, volumePct });
  }

  private async send(request: {
    command: string;
    player: string;
    volumePct?: number;
  }): Promise<void> {
    if (this.pending()) return;
    this.pending.set(true);
    this.error.set(null);
    try {
      // The response carries fresh state, so the bar re-renders immediately
      // rather than waiting for the change signal to make the round trip.
      const response = await this.api.sendMediaCommand(request);
      this.state.set(response.state);
    } catch (err: unknown) {
      this.error.set(problemDetail(err));
    } finally {
      this.pending.set(false);
    }
  }

  private connect(): void {
    if (this.closed || this.ws !== null) return;
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    this.ws = new WebSocket(`${protocol}//${window.location.host}/ws/v1`);

    this.ws.onmessage = (event) => {
      try {
        const envelope: EventEnvelope = JSON.parse(event.data);
        if (envelope.channel !== 'session' || envelope.type !== 'media.state') return;
        const payload = envelope.payload as { state?: MediaStateDto };
        if (payload.state !== undefined) {
          this.state.set(payload.state);
        }
      } catch {
        // A malformed frame is dropped; the next change corrects the display,
        // and the REST read is always available as the source of truth.
      }
    };

    this.ws.onclose = () => {
      this.ws = null;
      if (this.closed) return;
      this.reconnect = setTimeout(() => this.connect(), 3000);
    };
  }
}

/**
 * Pull the human-readable line out of an RFC 9457 problem body (docs/05 §7),
 * falling back to a neutral message. The server's `detail` is authored by
 * jarvisd — never by a player — so it is safe to show.
 */
function problemDetail(err: unknown): string {
  const body = (err as { error?: { detail?: unknown; title?: unknown } })?.error;
  if (typeof body?.detail === 'string') return body.detail;
  if (typeof body?.title === 'string') return body.title;
  return 'That media command did not go through.';
}

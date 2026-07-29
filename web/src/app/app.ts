import { KeyValuePipe } from '@angular/common';
import {
  Component,
  OnDestroy,
  OnInit,
  inject,
  signal,
  ChangeDetectionStrategy,
} from '@angular/core';
import { RouterLink, RouterOutlet } from '@angular/router';
import type { HealthResponse, SessionDto } from '../generated/api-types';
import { ApiService } from './api.service';
import { MediaBar } from './media-bar';
import { MediaService } from './media.service';

/**
 * Jarvis shell root (docs/03 §3). M0 scope: health page, first-run pairing,
 * and the session round-trip proving the persisted vertical slice (FR-02).
 * Conversation surfaces land in M1 (F1.8).
 *
 * The media bar (F3a.7, FR-22) lives at the shell root because it is ambient:
 * it controls whatever is playing regardless of which view is routed, and it is
 * absent entirely when nothing is.
 */
@Component({
  selector: 'app-root',
  imports: [KeyValuePipe, RouterLink, RouterOutlet, MediaBar],
  templateUrl: './app.html',
  changeDetection: ChangeDetectionStrategy.OnPush,
  styleUrl: './app.scss',
})
export class App implements OnInit, OnDestroy {
  private readonly api = inject(ApiService);
  protected readonly media = inject(MediaService);

  protected readonly title = signal('Jarvis');
  protected readonly health = signal<HealthResponse | null>(null);
  protected readonly sessions = signal<SessionDto[]>([]);
  protected readonly paired = signal(false);
  protected readonly error = signal<string | null>(null);
  protected readonly newSessionTitle = signal('');

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

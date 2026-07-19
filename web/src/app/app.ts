import { KeyValuePipe } from '@angular/common';
import { Component, OnInit, inject, signal, ChangeDetectionStrategy } from '@angular/core';
import type { HealthResponse, SessionDto } from '../generated/api-types';
import { ApiService } from './api.service';

/**
 * Jarvis shell root (docs/03 §3). M0 scope: health page, first-run pairing,
 * and the session round-trip proving the persisted vertical slice (FR-02).
 * Conversation surfaces land in M1.
 */
@Component({
  selector: 'app-root',
  imports: [KeyValuePipe],
  templateUrl: './app.html',
  changeDetection: ChangeDetectionStrategy.OnPush,
  styleUrl: './app.scss',
})
export class App implements OnInit {
  private readonly api = inject(ApiService);

  protected readonly title = signal('Jarvis');
  protected readonly health = signal<HealthResponse | null>(null);
  protected readonly sessions = signal<SessionDto[]>([]);
  protected readonly paired = signal(false);
  protected readonly error = signal<string | null>(null);
  protected readonly newSessionTitle = signal('');

  ngOnInit(): void {
    this.paired.set(this.api.hasToken());
    void this.refresh();
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

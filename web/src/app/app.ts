import { Component, signal } from '@angular/core';
import { RouterOutlet } from '@angular/router';
import type { ServiceStatus } from '../generated/api-types';

/**
 * Jarvis shell root (docs/03 §3). Placeholder until the health page and
 * session surfaces land in F0.8; state via Angular signals (docs/08 §6).
 * Wire types come exclusively from src/generated (ws-contracts skill) —
 * hand-written duplicates are a blocking review finding.
 */
@Component({
  selector: 'app-root',
  imports: [RouterOutlet],
  templateUrl: './app.html',
  styleUrl: './app.scss',
})
export class App {
  protected readonly title = signal('Jarvis');
  /** Until the health service polls jarvisd (F0.8), the shell is honest: degraded. */
  protected readonly status = signal<ServiceStatus>('degraded');
}

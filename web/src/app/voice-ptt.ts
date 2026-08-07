import { ChangeDetectionStrategy, Component, HostListener, OnDestroy, inject } from '@angular/core';
import { VoiceCaptureService } from './voice-capture.service';

@Component({
  selector: 'app-voice-ptt',
  changeDetection: ChangeDetectionStrategy.OnPush,
  template: `
    <button
      type="button"
      class="voice-ptt"
      [class.active]="voice.active()"
      [class.error]="voice.state() === 'error'"
      [disabled]="voice.state() === 'unavailable' || voice.state() === 'requesting'"
      [attr.aria-pressed]="voice.active()"
      [attr.aria-label]="label()"
      (pointerdown)="begin($event)"
      (pointerup)="end()"
      (pointercancel)="end()"
      (pointerleave)="end()"
      (keydown)="onKeydown($event)"
      (keyup)="onKeyup($event)"
    >
      <span class="voice-glyph" aria-hidden="true"></span>
      <span>{{ text() }}</span>
      @if (voice.droppedFrames() > 0) {
        <span class="voice-drop" aria-hidden="true">·</span>
      }
    </button>
    @if (voice.error(); as error) {
      <span class="voice-error" role="status">{{ error }}</span>
    }
  `,
  styles: [
    `
      :host { display: inline-flex; align-items: center; gap: 0.6ch; }
      .voice-ptt {
        display: inline-flex; align-items: center; gap: 0.7ch;
        min-block-size: 2.55rem; padding: 0.55rem 0.9rem;
        border: 1px solid var(--glass-border); border-radius: 999px;
        background: var(--glass-bg); color: var(--ink); font: inherit;
        font-size: clamp(11px, 1.5vmin, 14px); cursor: pointer;
        box-shadow: var(--glass-shadow); backdrop-filter: blur(var(--glass-blur));
      }
      .voice-ptt:hover:not(:disabled), .voice-ptt.active {
        border-color: var(--c-listen); color: var(--c-listen);
      }
      .voice-ptt.active { background: color-mix(in srgb, var(--c-listen) 10%, var(--glass-bg)); }
      .voice-ptt.error { border-color: color-mix(in srgb, var(--c-error) 45%, var(--glass-border)); }
      .voice-ptt:disabled { cursor: not-allowed; opacity: 0.68; }
      .voice-glyph {
        inline-size: 0.75rem; block-size: 1rem; border: 2px solid currentcolor;
        border-block-start: 0; border-radius: 0 0 0.6rem 0.6rem; position: relative;
      }
      .voice-glyph::after {
        content: ''; position: absolute; inset-inline-start: 50%; inset-block-end: -0.38rem;
        inline-size: 0.9rem; block-size: 2px; transform: translateX(-50%); background: currentcolor;
      }
      .voice-drop { color: var(--c-error); }
      .voice-error { max-inline-size: 16rem; color: var(--c-error); font-size: 0.75rem; }
      @media (prefers-reduced-motion: reduce) { .voice-ptt { transition: none; } }
    `,
  ],
})
export class VoicePtt implements OnDestroy {
  protected readonly voice = inject(VoiceCaptureService);
  private keyboardHeld = false;

  protected text(): string {
    switch (this.voice.state()) {
      case 'requesting': return 'Requesting mic…';
      case 'listening': return 'Listening · release to send';
      case 'error': return 'Try voice again';
      case 'unavailable': return 'Voice unavailable';
      default: return 'Hold to speak';
    }
  }

  protected label(): string {
    return this.voice.active() ? 'Release to stop voice capture' : 'Hold to speak';
  }

  protected begin(event: PointerEvent): void {
    if (event.currentTarget instanceof HTMLElement) {
      event.currentTarget.setPointerCapture(event.pointerId);
    }
    void this.voice.begin();
  }

  protected end(): void {
    if (!this.keyboardHeld) this.voice.end();
  }

  protected onKeydown(event: KeyboardEvent): void {
    if ((event.key === ' ' || event.key === 'Enter') && !event.repeat) {
      event.preventDefault();
      this.keyboardHeld = true;
      void this.voice.begin();
    }
  }

  protected onKeyup(event: KeyboardEvent): void {
    if (event.key === ' ' || event.key === 'Enter') {
      event.preventDefault();
      this.keyboardHeld = false;
      this.voice.end();
    }
  }

  @HostListener('window:blur')
  protected onWindowBlur(): void {
    this.keyboardHeld = false;
    this.voice.cancel();
  }

  ngOnDestroy(): void {
    this.voice.cancel();
  }
}

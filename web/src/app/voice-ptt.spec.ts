import { provideZonelessChangeDetection, signal, type WritableSignal } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { VoicePtt } from './voice-ptt';
import { VoiceCaptureService, type VoiceCaptureState } from './voice-capture.service';

describe('VoicePtt', () => {
  let fixture: ComponentFixture<VoicePtt>;
  let begin: jasmine.Spy;
  let end: jasmine.Spy;
  let state: WritableSignal<VoiceCaptureState>;
  let active: WritableSignal<boolean>;
  let droppedFrames: WritableSignal<number>;

  beforeEach(() => {
    begin = jasmine.createSpy('begin');
    end = jasmine.createSpy('end');
    state = signal<VoiceCaptureState>('idle');
    active = signal(false);
    const error = signal<string | null>(null);
    droppedFrames = signal(0);
    TestBed.configureTestingModule({
      imports: [VoicePtt],
      providers: [
        provideZonelessChangeDetection(),
        {
          provide: VoiceCaptureService,
          useValue: { state, active, error, droppedFrames, begin, end, cancel: end },
        },
      ],
    });
    fixture = TestBed.createComponent(VoicePtt);
    fixture.detectChanges();
  });

  it('exposes a keyboard-accessible hold-to-speak control', () => {
    const button = fixture.nativeElement.querySelector('button') as HTMLButtonElement;
    expect(button.textContent).toContain('Hold to speak');
    expect(button.getAttribute('aria-label')).toBe('Hold to speak');
    button.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter' }));
    button.dispatchEvent(new KeyboardEvent('keyup', { key: 'Enter' }));
    expect(begin).toHaveBeenCalledTimes(1);
    expect(end).toHaveBeenCalledTimes(1);
  });

  it('shows active listening language and capture backpressure feedback', () => {
    state.set('listening');
    active.set(true);
    droppedFrames.set(1);
    fixture.detectChanges();
    const button = fixture.nativeElement.querySelector('button') as HTMLButtonElement;
    expect(button.textContent).toContain('Listening');
    expect(button.getAttribute('aria-pressed')).toBe('true');
    expect(button.querySelector('.voice-drop')).not.toBeNull();
  });
});

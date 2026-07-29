import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import type { MediaPlayerDto, MediaStateDto } from '../generated/api-types';
import { MediaBar } from './media-bar';

function player(overrides: Partial<MediaPlayerDto> = {}): MediaPlayerDto {
  return {
    player: 'org.mpris.MediaPlayer2.spotify',
    identity: 'Spotify',
    status: 'playing',
    metadata: { title: 'Dancing Queen', artist: 'ABBA' },
    volumePct: 40,
    canPlay: true,
    canPause: true,
    canGoNext: true,
    canGoPrevious: true,
    canSeek: false,
    ...overrides,
  };
}

function state(overrides: Partial<MediaStateDto> = {}): MediaStateDto {
  return {
    players: [player()],
    activePlayer: 'org.mpris.MediaPlayer2.spotify',
    maxVolumePct: 70,
    ...overrides,
  };
}

describe('MediaBar', () => {
  let fixture: ComponentFixture<MediaBar>;

  function render(value: MediaStateDto, pending = false, error: string | null = null): HTMLElement {
    fixture = TestBed.createComponent(MediaBar);
    fixture.componentRef.setInput('state', value);
    fixture.componentRef.setInput('pending', pending);
    fixture.componentRef.setInput('error', error);
    fixture.detectChanges();
    return fixture.nativeElement as HTMLElement;
  }

  beforeEach(() => {
    TestBed.configureTestingModule({
      providers: [provideZonelessChangeDetection()],
    });
  });

  it('is absent entirely when nothing is playing', () => {
    // A dead shell is worse than no shell: with no players the bar renders
    // nothing at all.
    const el = render(state({ players: [], activePlayer: null }));
    expect(el.querySelector('.media-bar')).toBeNull();
  });

  it('shows the track and emits pause for the active player', (done) => {
    const el = render(state());
    expect(el.querySelector('.media-title')?.textContent).toContain('Dancing Queen');
    expect(el.querySelector('.media-artist')?.textContent).toContain('ABBA');

    fixture.componentInstance.transport.subscribe((event) => {
      // Exit evidence #4: the bar pauses whatever is playing.
      expect(event).toEqual({
        command: 'pause',
        player: 'org.mpris.MediaPlayer2.spotify',
      });
      done();
    });
    el.querySelector<HTMLButtonElement>('button[aria-label="Pause"]')!.click();
  });

  it('offers play (not pause) when the player is paused', () => {
    const el = render(
      state({ players: [player({ status: 'paused' })] }),
    );
    expect(el.querySelector('button[aria-label="Play"]')).not.toBeNull();
    expect(el.querySelector('button[aria-label="Pause"]')).toBeNull();
  });

  it('never guesses between two active players — it asks', () => {
    // ADR-016: the server sends no activePlayer when the choice is ambiguous,
    // and the bar must not default to players[0].
    const el = render(
      state({
        players: [player(), player({ player: 'org.mpris.MediaPlayer2.chromium', identity: 'Chromium' })],
        activePlayer: null,
      }),
    );

    expect(el.querySelector('.media-ask')).not.toBeNull();
    expect(el.querySelector('button[aria-label="Pause"]')).toBeNull();
    const choices = el.querySelectorAll('.media-choices button');
    expect(choices.length).toBe(2);

    // Choosing one resolves the ambiguity and the controls appear.
    (choices[1] as HTMLButtonElement).click();
    fixture.detectChanges();
    expect(el.querySelector('.media-ask')).toBeNull();
    expect(el.querySelector('button[aria-label="Pause"]')).not.toBeNull();
  });

  it('disables controls the player says it does not support', () => {
    const el = render(
      state({ players: [player({ canGoNext: false })] }),
    );
    const next = el.querySelector<HTMLButtonElement>('button[aria-label="Next track"]')!;
    expect(next.disabled).toBe(true);
    const previous = el.querySelector<HTMLButtonElement>('button[aria-label="Previous track"]')!;
    expect(previous.disabled).toBe(false);
  });

  it('blocks every control while a command is in flight', () => {
    const el = render(state(), true);
    for (const button of Array.from(el.querySelectorAll<HTMLButtonElement>('.media-controls button'))) {
      expect(button.disabled).toBe(true);
    }
  });

  it('clamps the volume slider to the configured cap', () => {
    const el = render(state());
    const slider = el.querySelector<HTMLInputElement>('input[type="range"]')!;
    // The UI cannot even offer above-cap values; the server enforces it anyway.
    expect(slider.max).toBe('70');
    expect(slider.value).toBe('40');
  });

  it('emits a clamped volume even if the control reports a higher value', (done) => {
    const el = render(state());
    const slider = el.querySelector<HTMLInputElement>('input[type="range"]')!;
    fixture.componentInstance.volume.subscribe((event) => {
      expect(event.volumePct).toBe(70);
      done();
    });
    slider.value = '95';
    slider.dispatchEvent(new Event('change'));
  });

  it('renders album art only when the player published one', () => {
    expect(render(state()).querySelector('.media-art')).toBeNull();

    const withArt = render(
      state({
        players: [
          player({
            metadata: {
              title: 'Dancing Queen',
              artist: 'ABBA',
              artUrl: 'https://cdn.example/art.jpg',
            },
          }),
        ],
      }),
    );
    expect(withArt.querySelector<HTMLImageElement>('.media-art')!.src).toBe(
      'https://cdn.example/art.jpg',
    );
  });

  it('renders player-published text as text, never as markup', () => {
    // Track metadata is Z4-untrusted: a hostile player must not be able to
    // inject markup into the shell through a song title.
    const el = render(
      state({
        players: [
          player({
            metadata: { title: '<img src=x onerror="alert(1)">', artist: 'ABBA' },
          }),
        ],
      }),
    );
    const title = el.querySelector('.media-title')!;
    expect(title.querySelector('img')).toBeNull();
    expect(title.textContent).toContain('<img src=x onerror="alert(1)">');
  });

  it('shows a server-supplied failure inline', () => {
    const el = render(state(), false, 'more than one player is active: name one in `player`');
    expect(el.querySelector('.media-error')?.textContent).toContain('more than one player');
  });
});

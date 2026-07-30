import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { NowPlayingCard, type NowPlayingCardData } from './now-playing-card';

describe('NowPlayingCard', () => {
  let fixture: ComponentFixture<NowPlayingCard>;
  let el: HTMLElement;

  function render(card: NowPlayingCardData): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(NowPlayingCard);
    fixture.componentRef.setInput('card', card);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  it('renders title, artist, album, and source app as plain text', () => {
    render({
      type: 'card.now_playing',
      id: 'card-6',
      title: 'Dancing Queen',
      artist: 'ABBA',
      album: 'Arrival',
      sourceApp: 'Spotify',
    });
    expect(el.textContent).toContain('Dancing Queen');
    expect(el.textContent).toContain('ABBA');
    expect(el.textContent).toContain('Arrival');
    expect(el.textContent).toContain('Spotify');
  });

  it('renders the player-published art with no source chip (docs/12 §2.3)', () => {
    render({
      type: 'card.now_playing',
      id: 'card-6',
      artUrl: 'https://cdn.example/art.jpg',
      sourceApp: 'Spotify',
    });
    const img = el.querySelector('img') as HTMLImageElement;
    expect(img.getAttribute('src')).toBe('https://cdn.example/art.jpg');
    expect(el.querySelector('app-source-chip')).toBeNull();
  });

  it('falls back to "Unknown track" when no title is available', () => {
    render({ type: 'card.now_playing', id: 'card-6', sourceApp: 'mpv' });
    expect(el.textContent).toContain('Unknown track');
  });
});

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

  // --- F5.7: what the player did not publish is not invented -------------

  it('omits the album line entirely when the player published no album', () => {
    render({
      type: 'card.now_playing',
      id: 'now-playing',
      title: 'Fade Into You',
      artist: 'Mazzy Star',
      sourceApp: 'mpv',
    });
    expect(el.querySelector('.now-playing-album')).toBeNull();
    expect(el.textContent).not.toContain('Unknown album');
    expect(el.textContent).toContain('Fade Into You');
    expect(el.textContent).toContain('Mazzy Star');
  });

  it('renders text-only with no placeholder image when there is no art', () => {
    render({
      type: 'card.now_playing',
      id: 'now-playing',
      title: 'Fade Into You',
      sourceApp: 'mpv',
    });
    // No <img> at all — a stand-in image would be a fabricated fact
    // (docs/12 §2.3, media-integration skill §9).
    expect(el.querySelector('img')).toBeNull();
    expect(el.querySelector('.now-playing-artist')).toBeNull();
  });

  it('renders every field as plain text, never as markup', () => {
    render({
      type: 'card.now_playing',
      id: 'now-playing',
      // Player-published metadata is Z4-untrusted (docs/06 §2).
      title: '<img src=x onerror="alert(1)">',
      artist: '<b>bold</b>',
      sourceApp: 'Chromium',
    });
    expect(el.querySelector('.now-playing-title b')).toBeNull();
    expect(el.querySelector('.now-playing-artist b')).toBeNull();
    expect(el.textContent).toContain('<b>bold</b>');
  });
});

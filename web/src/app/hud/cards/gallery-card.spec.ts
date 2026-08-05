import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { GalleryCard, type GalleryCardData } from './gallery-card';

describe('GalleryCard', () => {
  let fixture: ComponentFixture<GalleryCard>;
  let el: HTMLElement;

  function render(card: GalleryCardData): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(GalleryCard);
    fixture.componentRef.setInput('card', card);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  /** Two images that genuinely came from different pages — the ADR-017 case. */
  const mixedProvenance: GalleryCardData = {
    type: 'card.gallery',
    id: 'card-10',
    title: 'Pictures of ramen',
    images: [
      {
        url: 'https://cdn.a.example/1.jpg',
        sourceUrl: 'https://a.example/page',
        sourceDomain: 'a.example',
        alt: 'A bowl of shoyu ramen',
      },
      {
        url: 'https://cdn.b.example/2.jpg',
        sourceUrl: 'https://b.example/other',
        sourceDomain: 'b.example',
        alt: 'A bowl of miso ramen',
      },
    ],
  };

  it('badges every tile individually, with its own page — never one shared link', () => {
    render(mixedProvenance);

    const tiles = el.querySelectorAll('app-sourced-image');
    expect(tiles.length).toBe(2);

    const chips = el.querySelectorAll('app-source-chip a');
    expect(chips.length).toBe(2);
    // Provenance differs, so the badges differ (ADR-017): a single shared
    // attribution across these two images would be wrong.
    expect(chips[0].getAttribute('href')).toBe('https://a.example/page');
    expect(chips[0].textContent).toContain('a.example');
    expect(chips[1].getAttribute('href')).toBe('https://b.example/other');
    expect(chips[1].textContent).toContain('b.example');
  });

  it('gives every image its required alt text', () => {
    render(mixedProvenance);
    const images = el.querySelectorAll('img');
    expect(images.length).toBe(2);
    expect(images[0].getAttribute('alt')).toBe('A bowl of shoyu ramen');
    expect(images[1].getAttribute('alt')).toBe('A bowl of miso ramen');
  });

  it('shows exactly as many tiles as chips, so no image can appear unattributed', () => {
    render(mixedProvenance);
    expect(el.querySelectorAll('img').length).toBe(el.querySelectorAll('app-source-chip').length);
  });

  it('renders an empty gallery as a heading with no tiles rather than a broken grid', () => {
    render({ type: 'card.gallery', id: 'card-10', title: 'Pictures', images: [] });
    expect(el.textContent).toContain('Pictures');
    expect(el.querySelectorAll('app-sourced-image').length).toBe(0);
  });
});

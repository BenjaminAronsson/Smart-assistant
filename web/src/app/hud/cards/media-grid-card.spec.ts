import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { MediaGridCard, type MediaGridCardData } from './media-grid-card';

describe('MediaGridCard', () => {
  let fixture: ComponentFixture<MediaGridCard>;
  let el: HTMLElement;

  function render(card: MediaGridCardData): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(MediaGridCard);
    fixture.componentRef.setInput('card', card);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  it('renders every tile name and price as plain text', () => {
    render({
      type: 'card.media_grid',
      id: 'card-4',
      title: 'Menu',
      items: [
        { name: 'Tonkotsu', price: '$14' },
        { name: 'Miso', price: '$13' },
      ],
    });
    expect(el.textContent).toContain('Tonkotsu');
    expect(el.textContent).toContain('$14');
    expect(el.textContent).toContain('Miso');
  });

  it('renders the source chip on any tile carrying a photo', () => {
    render({
      type: 'card.media_grid',
      id: 'card-4',
      title: 'Menu',
      items: [
        {
          name: 'Tonkotsu',
          photo: {
            url: 'https://cdn.example/tonkotsu.jpg',
            sourceUrl: 'https://menu.example/tonkotsu',
            sourceDomain: 'menu.example',
            alt: 'Tonkotsu ramen bowl',
          },
        },
      ],
    });
    const chip = el.querySelector('app-source-chip');
    expect(chip).not.toBeNull();
    expect(chip?.textContent).toContain('menu.example');
  });
});

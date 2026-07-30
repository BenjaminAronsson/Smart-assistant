import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { PlaceCard, type PlaceCardData } from './place-card';

describe('PlaceCard', () => {
  let fixture: ComponentFixture<PlaceCard>;
  let el: HTMLElement;

  function render(card: PlaceCardData): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(PlaceCard);
    fixture.componentRef.setInput('card', card);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  it('renders the pills and name as plain text', () => {
    render({
      type: 'card.place',
      id: 'card-2',
      name: 'Kome Ramen',
      rating: '4.7',
      distance: '8 min',
      priceLevel: '$$',
      pick: false,
    });
    expect(el.textContent).toContain('Kome Ramen');
    expect(el.textContent).toContain('4.7');
    expect(el.textContent).toContain('8 min');
    expect(el.textContent).toContain('$$');
  });

  it('renders text-only with no photo when the card carries none', () => {
    render({
      type: 'card.place',
      id: 'card-2',
      name: 'Kome Ramen',
      pick: false,
    });
    expect(el.querySelector('app-sourced-image')).toBeNull();
  });

  it('renders the source chip whenever a photo is present (docs/12 §2.3)', () => {
    render({
      type: 'card.place',
      id: 'card-2',
      name: 'Kome Ramen',
      pick: false,
      photo: {
        url: 'https://cdn.example/ramen.jpg',
        sourceUrl: 'https://en.wikipedia.org/wiki/Ramen',
        sourceDomain: 'wikipedia.org',
        alt: 'A bowl of ramen',
      },
    });
    const chip = el.querySelector('app-source-chip');
    expect(chip).not.toBeNull();
    expect(chip?.textContent).toContain('wikipedia.org');
  });

  it('marks the pick variant with the host pick class (hue ring, docs/12 §2.3)', () => {
    render({
      type: 'card.place',
      id: 'card-2',
      name: 'Kome Ramen',
      pick: true,
    });
    expect(fixture.nativeElement.classList.contains('pick')).toBe(true);
    expect(el.textContent).toContain('Top pick');
  });
});

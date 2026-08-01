import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { provideHttpClient, withXhr } from '@angular/common/http';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import type { HudCardDto } from '../../../generated/api-types';
import { HudCard } from './hud-card';

describe('HudCard', () => {
  let fixture: ComponentFixture<HudCard>;
  let el: HTMLElement;
  let http: HttpTestingController;

  function render(card: HudCardDto, index = 0, reducedMotion = false): void {
    TestBed.configureTestingModule({
      providers: [
        provideZonelessChangeDetection(),
        provideHttpClient(withXhr()),
        provideHttpClientTesting(),
      ],
    });
    http = TestBed.inject(HttpTestingController);
    fixture = TestBed.createComponent(HudCard);
    fixture.componentRef.setInput('card', card);
    fixture.componentRef.setInput('index', index);
    fixture.componentRef.setInput('reducedMotion', reducedMotion);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  afterEach(() => http.verify());

  it('renders the registered sub-component for a value-readout card', () => {
    render({
      type: 'card.value_readout',
      id: 'card-1',
      label: 'Weather',
      value: '72°F',
      miniStats: [],
    });
    expect(el.querySelector('app-value-readout-card')).not.toBeNull();
    expect(el.querySelector('app-error-card')).toBeNull();
  });

  it('renders the registered sub-component for a place card', () => {
    render({ type: 'card.place', id: 'card-2', name: 'Kome Ramen', pick: false });
    expect(el.querySelector('app-place-card')).not.toBeNull();
  });

  it('renders the registered sub-component for a map card (F3b.5)', () => {
    render({
      type: 'card.map',
      id: 'card-9',
      label: 'Ramen Nagi',
      destination: { lon: -122.4194, lat: 37.7749 },
    });
    expect(el.querySelector('app-map-card')).not.toBeNull();
    // MapCard fetches its own coverage — flush it so `http.verify()` is clean.
    http.expectOne('/api/v1/map/coverage').flush(null, { status: 404, statusText: 'Not Found' });
  });

  // docs/12 §2.3/§9 acceptance: card grammar only — an unregistered type
  // degrades to the error card, never raw content.
  it('degrades an unrecognized discriminant to the error card', () => {
    const bogus = { type: 'card.time_machine', id: 'x' } as unknown as HudCardDto;
    render(bogus);
    expect(el.querySelector('app-error-card')).not.toBeNull();
    expect(el.textContent).toContain("This result can't be shown.");
    // No other sub-component ever mounts for an unknown type.
    for (const selector of [
      'app-value-readout-card',
      'app-place-card',
      'app-entity-card',
      'app-media-grid-card',
      'app-headlines-card',
      'app-now-playing-card',
      'app-map-card',
      'app-approval-card',
      'app-status-card',
    ]) {
      expect(el.querySelector(selector)).toBeNull();
    }
  });

  it('renders a genuine error card the same way', () => {
    render({ type: 'card.error', id: 'card-8', message: 'Could not load this result' });
    expect(el.textContent).toContain('Could not load this result');
  });

  // Invariant #1 / docs/12 §9: no model-authored HTML on the HUD face. Model
  // content lives only in narrow typed fields, rendered by Angular
  // interpolation. A field carrying markup-like text must still render as
  // inert text, never become a live element.
  it('renders a malicious-looking text field as visible text, never as markup', () => {
    render({
      type: 'card.entity',
      id: 'card-3',
      name: '<img src=x onerror=alert(1)>',
      facts: [],
    });
    expect(el.textContent).toContain('<img src=x onerror=alert(1)>');
    expect(el.querySelector('img')).toBeNull();
  });

  it('sets the stagger index and reduced-motion class used by the reveal animation', () => {
    render({ type: 'card.status', id: 'card-7', message: 'Queued', queued: true }, 2, true);
    expect(fixture.nativeElement.style.getPropertyValue('--card-index')).toBe('2');
    expect(fixture.nativeElement.classList.contains('reduced-motion')).toBe(true);
  });

  it('tags the host with the card type for styling/debugging', () => {
    render({ type: 'card.status', id: 'card-7', message: 'Queued', queued: false });
    expect(fixture.nativeElement.getAttribute('data-card-type')).toBe('card.status');
  });
});

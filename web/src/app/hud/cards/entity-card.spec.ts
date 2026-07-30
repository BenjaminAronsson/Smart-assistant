import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { EntityCard, type EntityCardData } from './entity-card';

describe('EntityCard', () => {
  let fixture: ComponentFixture<EntityCard>;
  let el: HTMLElement;

  function render(card: EntityCardData): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(EntityCard);
    fixture.componentRef.setInput('card', card);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  it('renders name, confidence, and facts as plain text', () => {
    render({
      type: 'card.entity',
      id: 'card-3',
      name: 'Ada Lovelace',
      confidencePct: 92,
      facts: ['Mathematician', 'Wrote the first algorithm'],
    });
    expect(el.textContent).toContain('Ada Lovelace');
    expect(el.textContent).toContain('92% confident');
    expect(el.textContent).toContain('Mathematician');
  });

  // Card content is never model-authored HTML (docs/12 §2.3/§9, invariant 1):
  // a "fact" carrying a would-be markup payload must render as inert text,
  // never as a live element the browser parses and executes.
  it('renders a fact containing markup-like text as visible text, never as an element', () => {
    render({
      type: 'card.entity',
      id: 'card-3',
      name: 'Ada Lovelace',
      facts: ['<img src=x onerror=alert(1)>'],
    });
    expect(el.textContent).toContain('<img src=x onerror=alert(1)>');
    // No <img> was ever created from this text — the only <img> a card can
    // ever contain comes from a SourcedImage, and none was supplied here.
    expect(el.querySelector('img')).toBeNull();
  });

  it('renders text-only with no photo when the card carries none', () => {
    render({ type: 'card.entity', id: 'card-3', name: 'Ada Lovelace' });
    expect(el.querySelector('app-sourced-image')).toBeNull();
  });
});

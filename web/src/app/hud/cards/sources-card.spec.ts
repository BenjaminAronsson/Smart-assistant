import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { SourcesCard, type SourcesCardData } from './sources-card';

describe('SourcesCard', () => {
  let fixture: ComponentFixture<SourcesCard>;
  let el: HTMLElement;

  function render(card: SourcesCardData): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(SourcesCard);
    fixture.componentRef.setInput('card', card);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  const twoSources: SourcesCardData = {
    type: 'card.sources',
    id: 'card-9',
    title: 'References',
    items: [
      {
        title: 'Ramen — Wikipedia',
        url: 'https://en.wikipedia.org/wiki/Ramen',
        domain: 'en.wikipedia.org',
      },
      {
        title: 'Berlin Ramen Guide',
        url: 'https://guide.example/ramen',
        domain: 'guide.example',
      },
    ],
  };

  it('lists every consulted page as a title with its own linked domain chip', () => {
    render(twoSources);
    expect(el.textContent).toContain('Ramen — Wikipedia');
    expect(el.textContent).toContain('Berlin Ramen Guide');

    const chips = el.querySelectorAll('app-source-chip a');
    expect(chips.length).toBe(2);
    expect(chips[0].getAttribute('href')).toBe('https://en.wikipedia.org/wiki/Ramen');
    expect(chips[0].textContent).toContain('en.wikipedia.org');
    expect(chips[1].getAttribute('href')).toBe('https://guide.example/ramen');
    expect(chips[1].textContent).toContain('guide.example');
  });

  it('renders the domain the server computed, never one derived from the url', () => {
    // A userinfo-spoofing URL is already labelled by its real host server-side;
    // the client must display that label as given rather than re-deriving one.
    render({
      type: 'card.sources',
      id: 'card-9',
      title: 'References',
      items: [
        {
          title: 'Totally Wikipedia',
          url: 'https://wikipedia.org@evil.example/x',
          domain: 'evil.example',
        },
      ],
    });
    const chip = el.querySelector('app-source-chip');
    expect(chip?.textContent).toContain('evil.example');
    expect(chip?.textContent).not.toContain('wikipedia.org ');
  });

  it('never renders page content — a reference is a link, not a reader', () => {
    // ADR-017 §3: reading a source is a browser handoff. The card has no field
    // for a page body, so the rendered output is titles and links only.
    render(twoSources);
    // Each item is exactly one title paragraph plus one chip; there is no
    // prose block that could hold a fetched page.
    expect(el.querySelectorAll('.source-item').length).toBe(2);
    expect(el.querySelectorAll('.source-item p').length).toBe(2);
    expect(el.querySelector('iframe')).toBeNull();
  });

  it('renders a markup-shaped page title as inert text', () => {
    // Page titles are untrusted (Z4): they render through interpolation only.
    render({
      type: 'card.sources',
      id: 'card-9',
      title: 'References',
      items: [
        {
          title: '<img src=x onerror=alert(1)>',
          url: 'https://example.org/x',
          domain: 'example.org',
        },
      ],
    });
    expect(el.textContent).toContain('<img src=x onerror=alert(1)>');
    expect(el.querySelector('img')).toBeNull();
  });

  it('links open safely in a new context', () => {
    render(twoSources);
    const link = el.querySelector('app-source-chip a');
    expect(link?.getAttribute('rel')).toContain('noopener');
    expect(link?.getAttribute('referrerpolicy')).toBe('no-referrer');
  });
});

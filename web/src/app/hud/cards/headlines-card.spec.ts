import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { HeadlinesCard, type HeadlinesCardData } from './headlines-card';

describe('HeadlinesCard', () => {
  let fixture: ComponentFixture<HeadlinesCard>;
  let el: HTMLElement;

  function render(card: HeadlinesCardData): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(HeadlinesCard);
    fixture.componentRef.setInput('card', card);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  it('renders every item — title, summary, relative time, source link — with no photo required', () => {
    render({
      type: 'card.headlines',
      id: 'card-5',
      title: 'World Cup',
      items: [
        {
          title: 'Final set for Sunday',
          summary: 'Two sides confirmed after semifinal wins.',
          relativeTime: '2h ago',
          sourceUrl: 'https://news.example/wc',
          sourceDomain: 'news.example',
        },
      ],
    });
    expect(el.textContent).toContain('Final set for Sunday');
    expect(el.textContent).toContain('Two sides confirmed after semifinal wins.');
    expect(el.textContent).toContain('2h ago');
    expect(el.querySelector('app-sourced-image')).toBeNull();
    const chip = el.querySelector('app-source-chip');
    expect(chip?.textContent).toContain('news.example');
  });

  it('renders a thumbnail with its own attribution when an item has one', () => {
    render({
      type: 'card.headlines',
      id: 'card-5',
      title: 'World Cup',
      items: [
        {
          title: 'Final set for Sunday',
          summary: 'Two sides confirmed.',
          relativeTime: '2h ago',
          sourceUrl: 'https://news.example/wc',
          sourceDomain: 'news.example',
          thumbnail: {
            url: 'https://cdn.example/stadium.jpg',
            sourceUrl: 'https://news.example/wc',
            sourceDomain: 'news.example',
            alt: 'Stadium at dusk',
          },
        },
      ],
    });
    expect(el.querySelector('app-sourced-image')).not.toBeNull();
  });
});

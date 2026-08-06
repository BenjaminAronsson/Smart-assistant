import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { AgendaCard, type AgendaCardData } from './agenda-card';

describe('AgendaCard', () => {
  let fixture: ComponentFixture<AgendaCard>;
  let el: HTMLElement;

  function render(card: AgendaCardData): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(AgendaCard);
    fixture.componentRef.setInput('card', card);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  it('renders timed and all-day events using only the safe event fields', () => {
    render({
      type: 'card.agenda',
      id: 'agenda-1',
      title: 'Today',
      events: [
        {
          title: 'Design review',
          start: '2026-08-06T09:30:00+02:00',
          end: '2026-08-06T10:00:00+02:00',
          allDay: false,
        },
        {
          title: 'Focus day',
          start: '2026-08-06',
          end: '2026-08-07',
          allDay: true,
        },
      ],
    });

    expect(el.textContent).toContain('Design review');
    expect(el.textContent).toContain('2026-08-06T09:30:00+02:00');
    expect(el.textContent).toContain('2026-08-06T10:00:00+02:00');
    expect(el.textContent).toContain('All day');
    expect(el.querySelectorAll('li').length).toBe(2);
  });

  it('keeps markup-shaped event text inert', () => {
    render({
      type: 'card.agenda',
      id: 'agenda-2',
      title: '<b>Today</b>',
      events: [
        { title: '<img src=x onerror=alert(1)>', start: 'start', end: 'end', allDay: false },
      ],
    });

    expect(el.textContent).toContain('<b>Today</b>');
    expect(el.textContent).toContain('<img src=x onerror=alert(1)>');
    expect(el.querySelector('b, img')).toBeNull();
  });
});

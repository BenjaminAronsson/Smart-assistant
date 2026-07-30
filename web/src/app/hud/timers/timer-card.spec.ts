import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import type { TimerDto } from '../../../generated/api-types';
import { TimerCard } from './timer-card';

function timer(overrides: Partial<TimerDto> = {}): TimerDto {
  return {
    id: '01ARZ3NDEKTSV4RRFFQ69G5FAV',
    name: 'pasta timer',
    kind: 'countdown',
    state: 'pending',
    fireAt: '2026-07-30T12:00:00Z',
    durationSecs: 600,
    remainingSecs: 599,
    ...overrides,
  } as TimerDto;
}

describe('TimerCard', () => {
  let fixture: ComponentFixture<TimerCard>;

  function render(dto: TimerDto, missed = false): HTMLElement {
    fixture = TestBed.createComponent(TimerCard);
    fixture.componentRef.setInput('timer', dto);
    fixture.componentRef.setInput('missed', missed);
    fixture.detectChanges();
    return fixture.nativeElement as HTMLElement;
  }

  beforeEach(() => {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
  });

  afterEach(() => fixture?.destroy());

  it('renders a live countdown with fixed-width digits', () => {
    const el = render(timer({ remainingSecs: 599 }));
    expect(el.querySelector('.countdown')?.textContent?.trim()).toBe('9:59');
    // docs/12: the number must not jitter as it ticks.
    const style = getComputedStyle(el.querySelector('.countdown') as HTMLElement);
    expect(style.fontVariantNumeric).toContain('tabular-nums');
  });

  it('formats hours, minutes and the zero floor', () => {
    expect(render(timer({ remainingSecs: 3723 })).querySelector('.countdown')?.textContent?.trim())
      .toBe('1:02:03');
    fixture.destroy();
    expect(render(timer({ remainingSecs: 0 })).querySelector('.countdown')?.textContent?.trim())
      .toBe('0:00');
  });

  it('ticks down locally instead of polling the server', () => {
    jasmine.clock().install();
    // Headless runners can report the page as hidden, which legitimately stops
    // the tick (docs/12 §6) — pin it visible so this test measures the tick.
    spyOnProperty(document, 'hidden', 'get').and.returnValue(false);
    try {
      const el = render(timer({ remainingSecs: 65 }));
      expect(el.querySelector('.countdown')?.textContent?.trim()).toBe('1:05');
      jasmine.clock().tick(5000);
      fixture.detectChanges();
      expect(el.querySelector('.countdown')?.textContent?.trim()).toBe('1:00');
    } finally {
      jasmine.clock().uninstall();
    }
  });

  it('holds at zero rather than counting negative', () => {
    jasmine.clock().install();
    spyOnProperty(document, 'hidden', 'get').and.returnValue(false);
    try {
      const el = render(timer({ remainingSecs: 2 }));
      jasmine.clock().tick(10_000);
      fixture.detectChanges();
      expect(el.querySelector('.countdown')?.textContent?.trim()).toBe('0:00');
    } finally {
      jasmine.clock().uninstall();
    }
  });

  it('announces a ringing timer in words, not by colour alone', () => {
    // docs/12 §8: state changes are announced by name. The live region carries
    // the whole meaning of the card.
    const el = render(timer({ state: 'fired', remainingSecs: null }));
    const live = el.querySelector('[aria-live="polite"]');
    expect(live?.textContent?.trim()).toBe('pasta timer is up');
    expect(el.className).toContain('state-fired');
    expect(el.querySelector('.countdown')).toBeNull();
  });

  it('says a missed alarm was missed (ADR-023) rather than letting it look fresh', () => {
    const el = render(timer({ state: 'fired', remainingSecs: null }), true);
    expect(el.querySelector('.missed')?.textContent?.trim()).toBe('Missed while offline');
    expect(el.querySelector('[aria-live="polite"]')?.textContent?.trim()).toBe(
      'Missed while offline. pasta timer is up',
    );
  });

  it('speaks a reminder by its note', () => {
    const el = render(
      timer({ kind: 'reminder', state: 'fired', note: 'call Mom', remainingSecs: null }),
    );
    expect(el.querySelector('.note')?.textContent?.trim()).toBe('call Mom');
    expect(el.querySelector('[aria-live="polite"]')?.textContent?.trim()).toBe(
      'Reminder — call Mom',
    );
  });

  it('offers only the affordances the state allows', () => {
    const labels = (el: HTMLElement) =>
      Array.from(el.querySelectorAll('button')).map((b) => b.textContent?.trim());

    expect(labels(render(timer({ state: 'pending' })))).toEqual(['Cancel']);
    fixture.destroy();
    expect(labels(render(timer({ state: 'fired', remainingSecs: null })))).toEqual([
      'Dismiss',
      'Snooze',
    ]);
    fixture.destroy();
    // A finished timer has nothing left to decide — no dead controls.
    expect(labels(render(timer({ state: 'dismissed', remainingSecs: null })))).toEqual([]);
  });

  it('emits dismiss / snooze / cancel with the timer id, and is keyboard reachable', () => {
    const el = render(timer({ state: 'fired', remainingSecs: null }));
    const emitted: string[] = [];
    fixture.componentInstance.dismiss.subscribe((id) => emitted.push(`dismiss:${id}`));
    fixture.componentInstance.snooze.subscribe((id) => emitted.push(`snooze:${id}`));

    const buttons = Array.from(el.querySelectorAll('button'));
    // Real <button>s: focusable and Enter/Space-activated natively, never
    // click-only divs (docs/12 §8).
    for (const button of buttons) {
      expect(button.tagName).toBe('BUTTON');
      expect(button.getAttribute('disabled')).toBeNull();
    }
    buttons[0].click();
    buttons[1].click();
    expect(emitted).toEqual([
      'dismiss:01ARZ3NDEKTSV4RRFFQ69G5FAV',
      'snooze:01ARZ3NDEKTSV4RRFFQ69G5FAV',
    ]);
  });

  it('blocks its controls while an action is in flight', () => {
    fixture = TestBed.createComponent(TimerCard);
    fixture.componentRef.setInput('timer', timer({ state: 'fired', remainingSecs: null }));
    fixture.componentRef.setInput('pending', true);
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;
    const emitted: string[] = [];
    fixture.componentInstance.dismiss.subscribe((id) => emitted.push(id));
    const button = el.querySelector('button') as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    button.click();
    expect(emitted).toEqual([]);
  });

  it('renders human text as text, never as markup', () => {
    // `name` and `note` are sanitized server-side but still human input; the
    // card grammar carries no free-form HTML (docs/12 §9).
    const el = render(
      timer({ name: '<img src=x onerror=alert(1)>', note: '<b>bold</b>', kind: 'reminder' }),
    );
    expect(el.querySelector('.name img')).toBeNull();
    expect(el.querySelector('.name')?.textContent).toContain('<img src=x');
    expect(el.querySelector('.note b')).toBeNull();
  });

  it('uses no fixed pixels for its layout scale', () => {
    // docs/12 §7: type is clamp(), spacing is vmin. A hard px font-size here
    // would break the 4K/portrait cases.
    const el = render(timer());
    const countdown = getComputedStyle(el.querySelector('.countdown') as HTMLElement);
    expect(countdown.fontSize).not.toBe('');
    const card = getComputedStyle(el.querySelector('.card') as HTMLElement);
    expect(card.backdropFilter || card.getPropertyValue('backdrop-filter')).toContain('blur');
  });

  it('stops its tick when the document is hidden (docs/12 §6)', () => {
    jasmine.clock().install();
    const hidden = spyOnProperty(document, 'hidden', 'get').and.returnValue(false);
    try {
      const el = render(timer({ remainingSecs: 100 }));
      hidden.and.returnValue(true);
      document.dispatchEvent(new Event('visibilitychange'));
      jasmine.clock().tick(10_000);
      fixture.detectChanges();
      expect(el.querySelector('.countdown')?.textContent?.trim()).toBe('1:40');
    } finally {
      jasmine.clock().uninstall();
    }
  });
});

import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { PRESENCE_HUE, PRESENCE_LABEL, type PresenceState } from './hud-state.service';
import { PresenceOrb } from './presence-orb';

const ALL_STATES: PresenceState[] = [
  'idle',
  'listening',
  'speaking',
  'tool',
  'waiting',
  'done',
  'error',
  'degraded',
];

describe('PresenceOrb', () => {
  let fixture: ComponentFixture<PresenceOrb>;

  function render(state: PresenceState, ambient = true): HTMLElement {
    fixture = TestBed.createComponent(PresenceOrb);
    fixture.componentRef.setInput('state', state);
    fixture.componentRef.setInput('ambient', ambient);
    fixture.detectChanges();
    return fixture.nativeElement as HTMLElement;
  }

  beforeEach(() => {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
  });

  it('carries every state as a class AND an announced name, never colour alone', () => {
    // docs/12 §2.1/§8: colour + motion + an announced state name. The class is
    // what drives the motion signature in CSS; the status text is what a screen
    // reader gets.
    for (const state of ALL_STATES) {
      const el = render(state);
      expect(el.className).toContain(`state-${state}`);
      expect(el.querySelector('[role="status"]')?.textContent?.trim()).toBe(
        PRESENCE_LABEL[state],
      );
    }
  });

  it('reserves amber for "waiting on you" (docs/12 §2.1 amber exclusivity)', () => {
    // The hue map is the single place a state picks a colour, so this assertion
    // is the amber-exclusivity check docs/12 §9 asks for: exactly one state may
    // reference --c-wait, and it is the one that wants a human decision.
    const amberStates = ALL_STATES.filter((s) => PRESENCE_HUE[s] === '--c-wait');
    expect(amberStates).toEqual(['waiting']);
  });

  it('is a keyboard-reachable button that opens the ops layer', (done) => {
    const el = render('idle');
    const button = el.querySelector('button.orb') as HTMLButtonElement;
    expect(button).not.toBeNull();
    // A <button> is focusable and Enter/Space activate it natively — the orb is
    // never a click-only div (docs/12 §8).
    expect(button.tagName).toBe('BUTTON');
    expect(button.getAttribute('aria-label')).toContain('operator layer');

    fixture.componentInstance.activate.subscribe(() => done());
    button.click();
  });

  it('stops ambient motion when the window is not active', () => {
    const el = render('idle', false);
    expect(el.className).toContain('motion-still');
    expect(render('idle', true).className).not.toContain('motion-still');
  });

  it('sizes itself in viewport units so it holds at any resolution', () => {
    // docs/12 §7: no fixed pixels in layout. The orb's size is a clamp() token,
    // not a hard-coded pixel value.
    const el = render('idle');
    const button = el.querySelector('button.orb') as HTMLElement;
    expect(getComputedStyle(button).inlineSize).not.toBe('');
    // The token itself must remain a clamp expression.
    const tokenValue = getComputedStyle(document.documentElement)
      .getPropertyValue('--orb-size')
      .trim();
    expect(tokenValue.startsWith('clamp(')).toBe(true);
  });
});

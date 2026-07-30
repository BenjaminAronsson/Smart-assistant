import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { Hud } from './hud';
import { HudStateService } from './hud-state.service';

describe('Hud', () => {
  let fixture: ComponentFixture<Hud>;
  let hud: HudStateService;
  let el: HTMLElement;

  beforeEach(() => {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    hud = TestBed.inject(HudStateService);
    hud.setReducedMotion(true); // deterministic caption reveal
    fixture = TestBed.createComponent(Hud);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  });

  afterEach(() => hud.stopReveal());

  it('is the front face: orb, caption and canvas — and no chat transcript', () => {
    expect(el.querySelector('app-presence-orb')).not.toBeNull();
    expect(el.querySelector('.caption')).not.toBeNull();
    expect(el.querySelector('.canvas')).not.toBeNull();
    // docs/12 §1: there is no transcript on the HUD face; it lives in the ops layer.
    expect(el.textContent).not.toContain('Sessions');
  });

  it('announces the caption politely and shows the spoken words', () => {
    const caption = el.querySelector('.caption') as HTMLElement;
    expect(caption.getAttribute('aria-live')).toBe('polite');

    hud.speak('Kome Ramen is eight minutes away.');
    fixture.detectChanges();
    expect(caption.textContent).toContain('Kome Ramen is eight minutes away.');
  });

  it('offers a keyboard-labelled route into the ops layer', () => {
    const toggle = el.querySelector('.ops-toggle') as HTMLButtonElement;
    expect(toggle.textContent).toContain('Ctrl');
    toggle.click();
    expect(hud.opsOpen()).toBe(true);
  });

  it('says the canvas is empty rather than rendering a silent blank', () => {
    expect(el.querySelector('.canvas-empty')?.textContent).toContain('Nothing on the canvas');
  });
});

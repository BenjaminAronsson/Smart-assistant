import { TestBed } from '@angular/core/testing';
import { provideZonelessChangeDetection } from '@angular/core';
import { hudCardId } from './cards/card-id';
import { HudStateService } from './hud-state.service';

describe('HudStateService', () => {
  let hud: HudStateService;

  beforeEach(() => {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    hud = TestBed.inject(HudStateService);
  });

  afterEach(() => hud.stopReveal());

  it('reveals a multi-sentence utterance one sentence at a time', () => {
    jasmine.clock().install();
    hud.speak('Kome Ramen is eight minutes away. It is rated four point seven.', 100);
    expect(hud.captionSentences().length).toBe(1);
    jasmine.clock().tick(101);
    expect(hud.captionSentences().length).toBe(2);
    // The reveal stops at the end rather than ticking forever.
    jasmine.clock().tick(500);
    expect(hud.captionSentences().length).toBe(2);
    jasmine.clock().uninstall();
  });

  it('lands the whole utterance at once under reduced motion', () => {
    hud.setReducedMotion(true);
    hud.speak('One. Two. Three.');
    expect(hud.captionSentences().length).toBe(3);
  });

  it('shows one utterance at a time — a new one replaces the last', () => {
    hud.setReducedMotion(true);
    hud.speak('First thing.');
    hud.speak('Second thing.');
    expect(hud.captionSentences()).toEqual(['Second thing.']);
  });

  it('stops ambient motion when hidden, reduced-motion, or battery-saving', () => {
    expect(hud.ambientMotion()).toBe(true);

    hud.setWindowActive(false);
    expect(hud.ambientMotion()).toBe(false);
    hud.setWindowActive(true);

    hud.setReducedMotion(true);
    expect(hud.ambientMotion()).toBe(false);
    hud.setReducedMotion(false);

    hud.setBatterySaver(true);
    expect(hud.ambientMotion()).toBe(false);
  });

  it('exposes the hue token and announced name for the current state', () => {
    hud.setPresence('waiting');
    expect(hud.hue()).toBe('--c-wait');
    expect(hud.presenceLabel()).toBe('Waiting on you');
  });

  it('toggles the ops layer', () => {
    expect(hud.opsOpen()).toBe(false);
    hud.toggleOps();
    expect(hud.opsOpen()).toBe(true);
    hud.setOpsOpen(false);
    expect(hud.opsOpen()).toBe(false);
  });

  it('starts with an empty canvas', () => {
    expect(hud.cards()).toEqual([]);
  });

  it('setCards replaces the canvas outright', () => {
    hud.setCards([{ type: 'card.status', id: 'a', message: 'Working', queued: false }]);
    expect(hud.cards().length).toBe(1);
    hud.setCards([{ type: 'card.status', id: 'b', message: 'Working again', queued: false }]);
    expect(hud.cards()).toEqual([
      { type: 'card.status', id: 'b', message: 'Working again', queued: false },
    ]);
  });

  it('appendCards extends the canvas without dropping what was there (FR-24 continuation)', () => {
    hud.setCards([{ type: 'card.status', id: 'a', message: 'First', queued: false }]);
    hud.appendCards([{ type: 'card.status', id: 'b', message: 'Second', queued: false }]);
    expect(hud.cards().map(hudCardId)).toEqual(['a', 'b']);
  });
});

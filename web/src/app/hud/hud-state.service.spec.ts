import { TestBed } from '@angular/core/testing';
import { provideZonelessChangeDetection } from '@angular/core';
import type { RunStateDto } from '../../generated/api-types';
import { hudCardId } from './cards/card-id';
import { HudStateService, type PresenceState, presenceForRunState } from './hud-state.service';

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

  it('tracks the active run position in the degraded queue', () => {
    hud.markRunQueued('run-a');
    expect(hud.queuePosition()).toBe(1);
    hud.markRunQueued('run-b');
    expect(hud.queuePosition()).toBe(2);
    hud.clearQueuedRun('run-a');
    expect(hud.queuePosition()).toBe(1);
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

  it('removes an approval only after its durable resolution', () => {
    const approval = {
      type: 'card.approval' as const,
      card: {
        approvalId: '01BX5ZZKBKACTAV9WEVGEMMVS1',
        runId: '01ARZ3NDEKTSV4RRFFQ69G5FAV',
        toolId: 'message.send',
        risk: 'r2' as const,
        egress: 'external' as const,
        reversible: false,
        exactEffect: 'Send an email',
        proposedArguments: {},
      },
    };
    hud.setCards([approval]);
    hud.resolveApproval(approval.card.approvalId);
    expect(hud.cards()).toEqual([]);
  });
});

describe('presenceForRunState', () => {
  // Exhaustive by construction (F9.11): a `RunStateDto` value not listed here
  // is a type error at the object literal, and `presenceForRunState`'s own
  // `never` default arm makes an unhandled variant a compile error at the
  // mapping's one seam — the pairing this milestone's own doc calls "the
  // compiler will not flag the miss" was previously true of, back when the
  // mapping was duplicated by hand in `App` and `Conversation`.
  const expected: Record<RunStateDto, PresenceState> = {
    received: 'speaking',
    context_ready: 'speaking',
    model_running: 'speaking',
    responding: 'speaking',
    replanning: 'speaking',
    tool_running: 'tool',
    waiting_approval: 'waiting',
    policy_review: 'waiting',
    completed: 'done',
    failed: 'error',
    cancelled: 'idle',
  };

  for (const [state, presence] of Object.entries(expected) as [RunStateDto, PresenceState][]) {
    it(`maps ${state} to ${presence}`, () => {
      expect(presenceForRunState(state)).toBe(presence);
    });
  }
});

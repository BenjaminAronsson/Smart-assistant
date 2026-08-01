import { TestBed } from '@angular/core/testing';
import { provideZonelessChangeDetection } from '@angular/core';
import type { HudCardDto } from '../../generated/api-types';
import { HudStateService } from './hud-state.service';
import { hudCardId } from './cards/card-id';

function card(id: string): HudCardDto {
  return { type: 'card.value_readout', id, label: 'Rating', value: '4.7', miniStats: [] };
}

function sources(id: string): HudCardDto {
  return {
    type: 'card.sources',
    id,
    title: 'References',
    items: [
      { title: 'Ramen', url: 'https://en.wikipedia.org/wiki/Ramen', domain: 'en.wikipedia.org' },
    ],
  };
}

/** Pending approval — exempt from shelving and TTL (docs/12 §4, F3b.4). */
function approval(approvalId: string): HudCardDto {
  return {
    type: 'card.approval',
    card: {
      approvalId,
      runId: '01ARZ3NDEKTSV4RRFFQ69G5FAV',
      toolId: 'message.send',
      risk: 'r2',
      egress: 'external',
      reversible: false,
      exactEffect: 'Send an email to the landlord',
      proposedArguments: {},
    },
  };
}

const ids = (cards: readonly HudCardDto[]): string[] => cards.map(hudCardId);

describe('Deep-dive canvas continuity (FR-27, ADR-017, docs/12 §2.5)', () => {
  let hud: HudStateService;

  beforeEach(() => {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    hud = TestBed.inject(HudStateService);
  });

  afterEach(() => hud.stopReveal());

  it('a continuation extends the canvas: new cards append, prior cards stay, nothing is shelved', () => {
    hud.setCards([card('ramen-1'), card('ramen-2')]);

    hud.routeTurn('extend', 'Ramen places', [sources('refs')]);

    expect(ids(hud.cards())).toEqual(['ramen-1', 'ramen-2', 'refs']);
    // The whole point of FR-27: a follow-up does not shelve.
    expect(hud.shelf().length).toBe(0);
  });

  it('several follow-ups in a row keep accumulating on one canvas', () => {
    hud.setCards([card('ramen-1')]);
    hud.routeTurn('extend', 'Ramen places', [card('ramen-2')]);
    hud.routeTurn('extend', 'Ramen places', [sources('refs')]);

    expect(ids(hud.cards())).toEqual(['ramen-1', 'ramen-2', 'refs']);
    expect(hud.shelf().length).toBe(0);
  });

  it('a genuine topic change shelves the thread and starts the canvas fresh', () => {
    hud.setCards([card('ramen-1'), card('ramen-2')]);

    hud.routeTurn('shelve', 'Ramen places', [card('weather')]);

    expect(ids(hud.cards())).toEqual(['weather']);
    expect(hud.shelf().length).toBe(1);
    expect(hud.shelf()[0].label).toBe('Ramen places');
    expect(ids(hud.shelf()[0].cards)).toEqual(['ramen-1', 'ramen-2']);
  });

  it('the shelved thread is restorable, so a misclassified boundary costs one keystroke', () => {
    // ADR-017's mitigation for classifier error: shelving is reversible.
    hud.setCards([card('ramen-1')]);
    hud.routeTurn('shelve', 'Ramen places', [card('weather')]);

    hud.restore(hud.shelf()[0].id);

    expect(ids(hud.cards())).toEqual(['ramen-1']);
  });

  it('a re-published card refreshes in place instead of appearing twice', () => {
    // The server publishes the live card set for a canvas, not a delta (F3b.6),
    // and the ids are stable — a thread's bibliography and a list card keep
    // theirs as they grow. So the same id arriving again is the same card.
    hud.setCards([card('ramen-1')]);
    hud.routeTurn('extend', 'Ramen places', [sources('refs')]);
    hud.routeTurn('extend', 'Ramen places', [sources('refs')]);

    expect(ids(hud.cards())).toEqual(['ramen-1', 'refs']);
  });

  it('a pending approval survives a continuation', () => {
    hud.setCards([approval('01BX5ZZKBKACTAV9WEVGEMMVS1'), card('ramen-1')]);

    hud.routeTurn('extend', 'Ramen places', [sources('refs')]);

    expect(ids(hud.cards())).toContain('01BX5ZZKBKACTAV9WEVGEMMVS1');
    expect(hud.shelf().length).toBe(0);
  });

  it('a pending approval survives a topic change and is never shelved (F3b.4 must not regress)', () => {
    hud.setCards([approval('01BX5ZZKBKACTAV9WEVGEMMVS1'), card('ramen-1')]);

    hud.routeTurn('shelve', 'Ramen places', [card('weather')]);

    // Still on the canvas: a new question does not retract a decision the human
    // still owes (docs/12 §4).
    expect(ids(hud.cards())).toContain('01BX5ZZKBKACTAV9WEVGEMMVS1');
    // And it is not on the shelf either — it was exempt, not moved.
    expect(ids(hud.shelf()[0].cards)).toEqual(['ramen-1']);
  });

  it('an approval is exempt no matter how many turns of either kind go by', () => {
    hud.setCards([approval('01BX5ZZKBKACTAV9WEVGEMMVS1')]);
    hud.routeTurn('extend', 'A', [card('a')]);
    hud.routeTurn('shelve', 'B', [card('b')]);
    hud.routeTurn('extend', 'B', [card('c')]);
    hud.routeTurn('shelve', 'C', [card('d')]);

    expect(ids(hud.cards())).toContain('01BX5ZZKBKACTAV9WEVGEMMVS1');
  });
});

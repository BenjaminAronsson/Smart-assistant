import { TestBed } from '@angular/core/testing';
import { provideZonelessChangeDetection } from '@angular/core';
import type { HudCardDto } from '../../generated/api-types';
import { HudStateService } from './hud-state.service';

/** A plain card that participates in the lifecycle. */
function card(id: string): HudCardDto {
  return { type: 'card.value_readout', id, label: 'Temperature', value: '21°C', miniStats: [] };
}

/** An approval card — exempt from shelving, clear-all and TTL (docs/12 §4). */
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

describe('Panel lifecycle (FR-24, docs/12 §4)', () => {
  let hud: HudStateService;

  beforeEach(() => {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    hud = TestBed.inject(HudStateService);
  });

  afterEach(() => hud.stopReveal());

  it('shelves the canvas on a new topic and restores it', () => {
    hud.setCards([card('a'), card('b')]);
    hud.newQuery('Ramen places');

    expect(hud.cards()).toEqual([]);
    expect(hud.shelf().length).toBe(1);
    expect(hud.shelf()[0].label).toBe('Ramen places');

    hud.restore(hud.shelf()[0].id);
    expect(hud.cards().map((c) => (c as { id: string }).id)).toEqual(['a', 'b']);
    expect(hud.shelf().length).toBe(0);
  });

  it('restoring shelves whatever was on the canvas (a swap, not a replace)', () => {
    hud.setCards([card('old')]);
    hud.newQuery('First topic');
    hud.setCards([card('new')]);

    hud.restore(hud.shelf()[0].id);

    expect(hud.cards().map((c) => (c as { id: string }).id)).toEqual(['old']);
    expect(hud.shelf().length).toBe(1);
    expect(hud.shelf()[0].cards.map((c) => (c as { id: string }).id)).toEqual(['new']);
  });

  it('holds at most four shelved panels, dropping the oldest', () => {
    for (let i = 1; i <= 6; i++) {
      hud.setCards([card(`c${i}`)]);
      hud.newQuery(`Topic ${i}`);
    }
    expect(hud.shelf().length).toBe(4);
    expect(hud.shelf().map((p) => p.label)).toEqual([
      'Topic 3',
      'Topic 4',
      'Topic 5',
      'Topic 6',
    ]);
  });

  it('never shelves a pending approval — a new question does not retract it', () => {
    hud.setCards([card('a'), approval('ap-1')]);
    hud.newQuery('Something else');

    // The approval stays on the canvas; only the ordinary card was shelved.
    expect(hud.cards().length).toBe(1);
    expect(hud.cards()[0].type).toBe('card.approval');
    expect(hud.shelf()[0].cards.map((c) => (c as { id: string }).id)).toEqual(['a']);
  });

  it('clear-all keeps pending approvals', () => {
    hud.setCards([card('a'), approval('ap-1')]);
    hud.newQuery('Topic');
    hud.setCards([...hud.cards(), card('b')]);

    hud.clearAll();

    expect(hud.shelf()).toEqual([]);
    expect(hud.cards().length).toBe(1);
    expect(hud.cards()[0].type).toBe('card.approval');
  });

  it('dismisses a single card and a single shelf chip', () => {
    hud.setCards([card('a'), card('b')]);
    hud.dismissCard('a');
    expect(hud.cards().map((c) => (c as { id: string }).id)).toEqual(['b']);

    hud.newQuery('Topic');
    hud.dismissShelf(hud.shelf()[0].id);
    expect(hud.shelf()).toEqual([]);
  });

  it('expires displayed and shelved panels silently after the TTL, approvals exempt', () => {
    const t0 = 1_000_000;
    hud.setCards([card('a'), approval('ap-1')], );
    hud.newQuery('Topic', t0);
    hud.setCards([card('fresh')]);

    // Just before the 2h default TTL: nothing has gone.
    const almost = t0 + 2 * 60 * 60 * 1000 - 1;
    hud.sweepExpired(almost);
    expect(hud.shelf().length).toBe(1);

    // Past it: the shelved panel is simply gone — no event, no animation.
    const after = t0 + 2 * 60 * 60 * 1000 + 1;
    hud.sweepExpired(after);
    expect(hud.shelf().length).toBe(0);
    // The approval survives its own TTL: it persists until decided.
    expect(hud.cards().some((c) => c.type === 'card.approval')).toBe(true);
  });

  it('honours a configured panel_ttl_hours', () => {
    hud.setPanelTtlHours(1);
    const t0 = 5_000_000;
    hud.setCards([card('a')]);
    hud.newQuery('Topic', t0);

    hud.sweepExpired(t0 + 61 * 60 * 1000);
    expect(hud.shelf().length).toBe(0);
  });

  it('rejects a nonsense TTL rather than expiring everything instantly', () => {
    hud.setPanelTtlHours(0);
    hud.setPanelTtlHours(-3);
    hud.setPanelTtlHours(Number.NaN);

    const t0 = 9_000_000;
    hud.setCards([card('a')]);
    hud.newQuery('Topic', t0);
    hud.sweepExpired(t0 + 60 * 1000);
    expect(hud.shelf().length).toBe(1);
  });
});

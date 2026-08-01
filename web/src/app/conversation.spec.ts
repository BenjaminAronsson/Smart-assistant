import { provideZonelessChangeDetection } from '@angular/core';
import { TestBed } from '@angular/core/testing';
import { ActivatedRoute } from '@angular/router';
import type { EventEnvelope, HudCanvasDto, HudCardDto } from '../generated/api-types';
import { ApiService } from './api.service';
import { Conversation } from './conversation';
import { HudStateService } from './hud/hud-state.service';

const THIS_SESSION = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
const OTHER_SESSION = '01BX5ZZKBKACTAV9WEVGEMMVRY';

function sourcesCard(id: string): HudCardDto {
  return {
    type: 'card.sources',
    id,
    title: 'References',
    items: [
      {
        title: 'Ramen — Wikipedia',
        url: 'https://en.wikipedia.org/wiki/Ramen',
        domain: 'en.wikipedia.org',
      },
    ],
  } as unknown as HudCardDto;
}

function canvasEnvelope(canvas: HudCanvasDto): EventEnvelope {
  return {
    channel: 'session',
    type: 'hud.canvas',
    payload: { canvas },
    seq: 1,
    occurredAt: '2026-01-01T00:00:00Z',
    v: 1,
  } as unknown as EventEnvelope;
}

/**
 * The private WS entry point, reached the same way the socket reaches it. The
 * cast is deliberate and narrow: routing a canvas instruction is a security
 * decision made inside `handleWebSocketMessage`, so the test drives exactly
 * that function rather than a public re-implementation of it.
 */
interface ConversationInternals {
  sessionId: string | null;
  handleWebSocketMessage(env: EventEnvelope): void;
}

describe('Conversation — hud.canvas session scoping (F3b.6)', () => {
  let component: ConversationInternals;
  let hud: HudStateService;
  let routeTurn: jasmine.Spy;

  beforeEach(() => {
    TestBed.configureTestingModule({
      providers: [
        provideZonelessChangeDetection(),
        // Stubbed: the component is never initialized in these tests, so no
        // request and no socket is ever opened. `handleWebSocketMessage` is
        // pure with respect to both.
        { provide: ApiService, useValue: {} },
        { provide: ActivatedRoute, useValue: { snapshot: { paramMap: new Map() } } },
      ],
    });
    // Created but deliberately NOT `detectChanges()`d: `ngOnInit` would fetch
    // the session and open a WebSocket, neither of which this behaviour needs.
    const fixture = TestBed.createComponent(Conversation);
    component = fixture.componentInstance as unknown as ConversationInternals;
    component.sessionId = THIS_SESSION;
    hud = TestBed.inject(HudStateService);
    routeTurn = spyOn(hud, 'routeTurn');
  });

  it('applies a canvas instruction addressed to the conversation on screen', () => {
    component.handleWebSocketMessage(
      canvasEnvelope({
        sessionId: THIS_SESSION,
        action: 'extend',
        label: 'Ramen places',
        cards: [sourcesCard('deepdive-sources-a')],
      }),
    );

    expect(routeTurn).toHaveBeenCalledTimes(1);
    const [action, label, cards] = routeTurn.calls.mostRecent().args;
    expect(action).toBe('extend');
    expect(label).toBe('Ramen places');
    expect((cards as HudCardDto[]).length).toBe(1);
  });

  it('drops a canvas instruction belonging to another session', () => {
    // The WS fan-out is global — one broadcast to every authenticated device
    // and every open conversation — so `sessionId` in the payload is the ONLY
    // thing scoping a canvas instruction. Ignoring it rendered session B's
    // sources and gallery cards onto session A's canvas.
    component.handleWebSocketMessage(
      canvasEnvelope({
        sessionId: OTHER_SESSION,
        action: 'extend',
        label: "another conversation's topic",
        cards: [sourcesCard('deepdive-sources-b')],
      }),
    );

    expect(routeTurn).not.toHaveBeenCalled();
  });

  it("does not let another session's topic change shelve this canvas", () => {
    // `shelve` is the destructive half: it collapses the panels this
    // conversation is showing into a shelf chip. A topic change in a different
    // conversation must not do that here.
    component.handleWebSocketMessage(
      canvasEnvelope({
        sessionId: OTHER_SESSION,
        action: 'shelve',
        label: 'displaced somewhere else',
        cards: [],
      }),
    );

    expect(routeTurn).not.toHaveBeenCalled();
  });

  it('applies a canvas instruction that names no session at all', () => {
    // Not an unscoped leak but the documented "applies anywhere" case: a list
    // card produced by the deterministic list grammar (FR-34) has no session,
    // so `sessionId` is absent and the guard must let it through. A guard that
    // required a match would silently stop every list card from rendering.
    component.handleWebSocketMessage(
      canvasEnvelope({
        action: 'extend',
        label: 'Shopping',
        cards: [],
      }),
    );
    expect(routeTurn).toHaveBeenCalledTimes(1);

    // Explicit `null` is the same case as absent — the field is
    // `Option<SessionId>` on the wire and serializes either way.
    component.handleWebSocketMessage(
      canvasEnvelope({
        sessionId: null,
        action: 'extend',
        label: 'Shopping',
        cards: [],
      }),
    );
    expect(routeTurn).toHaveBeenCalledTimes(2);
  });

  it('ignores a canvas instruction on a channel this view does not read', () => {
    // The channel check precedes the scoping check; asserted so a future
    // refactor cannot reorder them into a leak.
    const env = canvasEnvelope({
      sessionId: THIS_SESSION,
      action: 'extend',
      label: 'Ramen places',
      cards: [],
    });
    component.handleWebSocketMessage({ ...env, channel: 'system' } as unknown as EventEnvelope);
    expect(routeTurn).not.toHaveBeenCalled();
  });
});

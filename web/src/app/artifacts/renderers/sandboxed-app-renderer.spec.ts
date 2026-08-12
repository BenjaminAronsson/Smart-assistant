import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { provideHttpClient } from '@angular/common/http';
import { CAPABILITY_REQUEST, CAPABILITY_RESULT } from '../app-bridge.service';
import { SandboxedAppRenderer } from './sandboxed-app-renderer';

describe('SandboxedAppRenderer (F6.4, ADR-030)', () => {
  let fixture: ComponentFixture<SandboxedAppRenderer>;
  let el: HTMLElement;

  let http: HttpTestingController;

  function render(document: string): void {
    TestBed.configureTestingModule({
      providers: [
        provideZonelessChangeDetection(),
        provideHttpClient(),
        provideHttpClientTesting(),
      ],
    });
    http = TestBed.inject(HttpTestingController);
    fixture = TestBed.createComponent(SandboxedAppRenderer);
    fixture.componentRef.setInput('document', document);
    fixture.componentRef.setInput('artifactId', 'art-1');
    fixture.componentRef.setInput('version', 1);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  const APP = '<meta http-equiv="Content-Security-Policy" content="default-src \'none\'"><h1>hi</h1>';

  /**
   * The one assertion this whole feature rests on. `allow-scripts` WITHOUT
   * `allow-same-origin` is what gives the app an opaque origin; adding
   * `allow-same-origin` — the obvious "fix" for an app that cannot reach
   * localStorage — would let the frame remove its own sandbox and read the
   * device token. If this test is ever edited to accommodate a new token, the
   * change is a security decision, not a test fix.
   */
  it('sandboxes the frame with allow-scripts and nothing else', () => {
    render(APP);
    const frame = el.querySelector('iframe');
    expect(frame).not.toBeNull();
    expect(frame?.getAttribute('sandbox')).toBe('allow-scripts');
    expect(frame?.getAttribute('sandbox')).not.toContain('allow-same-origin');
  });

  it('passes the document through srcdoc, never into this page', () => {
    render(APP);
    const frame = el.querySelector('iframe');
    expect(frame?.getAttribute('srcdoc')).toContain('<h1>hi</h1>');
    // The app's markup must exist only inside the frame's srcdoc attribute —
    // never as elements of the control origin's DOM.
    expect(el.querySelector('h1')).toBeNull();
  });

  it('does not execute app script in the control origin', () => {
    render('<script>window.__jarvisEscaped = true;</script><p>x</p>');
    expect(el.querySelector('script')).toBeNull();
    expect((window as unknown as Record<string, unknown>)['__jarvisEscaped']).toBeUndefined();
  });

  it('never sets a src that could inherit an origin, and leaks no referrer', () => {
    render(APP);
    const frame = el.querySelector('iframe');
    expect(frame?.getAttribute('src')).toBeNull();
    expect(frame?.getAttribute('referrerpolicy')).toBe('no-referrer');
  });

  it('labels the frame for assistive technology', () => {
    TestBed.configureTestingModule({
      providers: [
        provideZonelessChangeDetection(),
        provideHttpClient(),
        provideHttpClientTesting(),
      ],
    });
    fixture = TestBed.createComponent(SandboxedAppRenderer);
    fixture.componentRef.setInput('document', APP);
    fixture.componentRef.setInput('artifactId', 'art-1');
    fixture.componentRef.setInput('version', 1);
    fixture.componentRef.setInput('label', 'Generated app 01ABC, version 1');
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
    expect(el.querySelector('iframe')?.getAttribute('title')).toBe(
      'Generated app 01ABC, version 1',
    );
  });

  // --- the bridge (F6.5) ---------------------------------------------------

  /** An opaque-origin frame's message: `origin` is the literal "null", and the
   * `source` is shadowed onto the instance because `MessageEvent`'s init
   * dictionary only accepts a real `EventTarget`. */
  function post(source: Window | null, data: unknown): void {
    const event = new MessageEvent('message', { data, origin: 'null' });
    Object.defineProperty(event, 'source', { get: () => source, configurable: true });
    window.dispatchEvent(event);
  }

  /**
   * A real cross-origin `contentWindow` is neither spy-able nor readable, which
   * is the point of the sandbox — so the test shadows the getter with a stub
   * that records what the component posts. The identity check under test is
   * `event.source === frame.contentWindow`, and that still holds exactly.
   */
  /** Let the component's own async handler finish after the last HTTP flush. */
  function settle(): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, 0));
  }

  function stubFrameWindow(): { source: Window; posted: unknown[] } {
    const frame = el.querySelector('iframe') as HTMLIFrameElement;
    const posted: unknown[] = [];
    const source = {
      postMessage: (message: unknown) => posted.push(message),
    } as unknown as Window;
    Object.defineProperty(frame, 'contentWindow', { get: () => source, configurable: true });
    return { source, posted };
  }

  /**
   * A message from **any window other than this frame** is ignored — silently,
   * so a page cannot learn whether an app is open by probing. An opaque-origin
   * frame posts with `origin: "null"`, which every sandboxed frame shares, so
   * source identity is the only thing that can distinguish them (ADR-030).
   */
  it('ignores a capability request that did not come from its own frame', async () => {
    render(APP);
    post(window, { type: CAPABILITY_REQUEST, id: '1', capability: 'home.read_state', target: 'x' });
    await fixture.whenStable();
    // http.verify() in afterEach fails if any call was made.
    http.verify();
  });

  it('ignores a message from its own frame that is not a capability request', async () => {
    render(APP);
    const frame = el.querySelector('iframe') as HTMLIFrameElement;
    post(frame.contentWindow, { type: 'jarvis.something.else', id: '1' });
    post(frame.contentWindow, { type: CAPABILITY_REQUEST, id: 7 });
    await fixture.whenStable();
    http.verify();
  });

  /**
   * The whole loop: a well-formed request from this frame mints a single-use
   * token and exchanges it, and the reply is posted back into the frame. Note
   * what the shell does NOT do — it never decides; jarvisd re-checks the
   * manifest, runs policy and mints any grant.
   */
  it('mints a token, exchanges it, and posts the result back into the frame', async () => {
    render(APP);
    const { source, posted } = stubFrameWindow();

    post(source, {
      type: CAPABILITY_REQUEST,
      id: 'req-1',
      capability: 'home.read_state',
      target: 'sensor.kitchen_temperature',
    });
    await fixture.whenStable();

    const mint = http.expectOne('/api/v1/apps/art-1/versions/1/capability-tokens');
    expect(mint.request.body).toEqual({ capability: 'home.read_state' });
    mint.flush({ token: 'ab'.repeat(32), expiresAt: '2026-01-01T00:00:00Z', capability: 'home.read_state' });
    await fixture.whenStable();

    const invoke = http.expectOne('/api/v1/apps/art-1/versions/1/invoke');
    expect(invoke.request.body.token).toBe('ab'.repeat(32));
    expect(invoke.request.body.target).toBe('sensor.kitchen_temperature');
    invoke.flush({ content: '21.5', truncated: false });
    await settle();

    expect(posted).toEqual([
      { type: CAPABILITY_RESULT, id: 'req-1', ok: true, content: '21.5' },
    ]);
  });

  /**
   * A refusal is a normal outcome an app must be able to render — and only the
   * stable machine code crosses back, never a server-authored sentence that
   * would read as the shell speaking inside a generated app.
   */
  it('returns the machine code when the host refuses, and nothing else', async () => {
    render(APP);
    const { source, posted } = stubFrameWindow();

    post(source, {
      type: CAPABILITY_REQUEST,
      id: 'req-2',
      capability: 'home.set_light',
      target: 'light.kitchen',
      value: 'on',
    });
    await fixture.whenStable();
    http.expectOne('/api/v1/apps/art-1/versions/1/capability-tokens').flush(
      {
        type: 'about:blank',
        title: 'this app does not declare that capability',
        status: 403,
        code: 'app.undeclared_capability',
      },
      { status: 403, statusText: 'Forbidden' },
    );
    await settle();

    expect(posted).toEqual([
      { type: CAPABILITY_RESULT, id: 'req-2', ok: false, code: 'app.undeclared_capability' },
    ]);
  });
});

import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { SandboxedAppRenderer } from './sandboxed-app-renderer';

describe('SandboxedAppRenderer (F6.4, ADR-030)', () => {
  let fixture: ComponentFixture<SandboxedAppRenderer>;
  let el: HTMLElement;

  function render(document: string): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(SandboxedAppRenderer);
    fixture.componentRef.setInput('document', document);
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
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(SandboxedAppRenderer);
    fixture.componentRef.setInput('document', APP);
    fixture.componentRef.setInput('label', 'Generated app 01ABC, version 1');
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
    expect(el.querySelector('iframe')?.getAttribute('title')).toBe(
      'Generated app 01ABC, version 1',
    );
  });
});

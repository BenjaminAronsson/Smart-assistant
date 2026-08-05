import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { MapGlView } from './map-gl-view';

/**
 * Deliberately light: the style/marker/bounds *decisions* this component
 * hands to MapLibre are covered exhaustively, without a browser GL context,
 * by `map-geo.spec.ts` and `map-style.spec.ts`. This only checks the
 * Angular wiring — inputs bind, the container renders, and creating/
 * destroying the component never throws synchronously — without waiting for
 * (or asserting on) the async `import('maplibre-gl')` + real WebGL
 * initialization it kicks off, which is out of scope for a deterministic
 * unit test.
 */
describe('MapGlView', () => {
  let fixture: ComponentFixture<MapGlView>;

  function setup(): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(MapGlView);
    fixture.componentRef.setInput('mode', 'local');
    fixture.componentRef.setInput('tileUrlTemplate', '/api/v1/map/tiles/{z}/{x}/{y}');
    fixture.componentRef.setInput('tileFormat', 'mvt');
    fixture.componentRef.setInput('attribution', '© OpenStreetMap contributors');
    fixture.componentRef.setInput('destination', { lon: -122.4194, lat: 37.7749 });
  }

  it('renders an aria-hidden container without throwing', () => {
    setup();
    expect(() => fixture.detectChanges()).not.toThrow();
    const container = (fixture.nativeElement as HTMLElement).querySelector('.map-gl-container');
    expect(container).not.toBeNull();
    expect(container?.getAttribute('aria-hidden')).toBe('true');
  });

  it('destroys cleanly even before the async GL mount has resolved', () => {
    setup();
    fixture.detectChanges();
    expect(() => fixture.destroy()).not.toThrow();
  });

  // S4: attribution must render as plain text (a normal Angular text
  // interpolation) and never be interpreted as markup — it is not passed to
  // MapLibre's own `AttributionControl`, which would write it through an
  // `innerHTML` sink.
  it('renders the attribution as plain text', () => {
    setup();
    fixture.detectChanges();
    const el = (fixture.nativeElement as HTMLElement).querySelector('.map-attribution');
    expect(el).not.toBeNull();
    expect(el?.textContent).toBe('© OpenStreetMap contributors');
  });

  it('does not interpret markup in a hostile attribution string', () => {
    setup();
    const hostile = '<img src=x onerror="window.__pwned = true"><b>bold</b> & "quotes"';
    fixture.componentRef.setInput('attribution', hostile);
    fixture.detectChanges();
    const el = (fixture.nativeElement as HTMLElement).querySelector('.map-attribution');
    expect(el).not.toBeNull();
    // Rendered verbatim as text content...
    expect(el?.textContent).toBe(hostile);
    // ...never parsed into child elements.
    expect(el?.querySelector('img')).toBeNull();
    expect(el?.querySelector('b')).toBeNull();
    expect(el?.children.length).toBe(0);
  });
});

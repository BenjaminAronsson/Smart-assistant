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
});

import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { provideHttpClient, withXhr } from '@angular/common/http';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import type { MapCardData } from './map-card';
import { MapCard } from './map-card';

const IN_REGION_DESTINATION = { lon: -122.4194, lat: 37.7749 };
const OUT_OF_REGION_DESTINATION = { lon: 103.866667, lat: 13.4125 };
const SF_BOUNDS = { minLon: -122.6, minLat: 37.6, maxLon: -122.3, maxLat: 37.85 };

function card(overrides: Partial<MapCardData> = {}): MapCardData {
  return {
    type: 'card.map',
    id: 'card-9',
    label: 'Ramen Nagi',
    destination: IN_REGION_DESTINATION,
    ...overrides,
  };
}

describe('MapCard (F3b.5, docs/12 §3, ADR-013)', () => {
  let fixture: ComponentFixture<MapCard>;
  let http: HttpTestingController;
  let el: HTMLElement;

  function setup(data: MapCardData): void {
    TestBed.configureTestingModule({
      providers: [
        provideZonelessChangeDetection(),
        provideHttpClient(withXhr()),
        provideHttpClientTesting(),
      ],
    });
    http = TestBed.inject(HttpTestingController);
    fixture = TestBed.createComponent(MapCard);
    fixture.componentRef.setInput('card', data);
    el = fixture.nativeElement as HTMLElement;
  }

  afterEach(() => http.verify());

  // Choreography note: every test below does exactly one `fixture.detectChanges()`
  // (never `await fixture.whenStable()`) between `setup()` and the coverage
  // `flush()`. `MapCard`'s coverage lookup is a `resource()`
  // (docs on `MapCard` itself), and `resource()` deliberately holds a
  // `PendingTasks` entry open for the whole in-flight load — that's what
  // makes `whenStable()` a safe, non-racy signal *after* the response is
  // flushed (see below), but it also means awaiting `whenStable()` *before*
  // the flush would deadlock: nothing ever completes the pending task until
  // the mocked request is flushed, and nothing flushes it while the test is
  // stuck awaiting stability. `detectChanges()` alone is sufficient to run
  // the resource's load effect and open the `HttpTestingController` request
  // (Angular runs pending effects as part of `ApplicationRef.tick()`, which
  // zoneless `detectChanges()` calls directly), so it's all that's needed
  // pre-flush.
  it('shows a loading placeholder before the coverage lookup resolves', async () => {
    setup(card());
    fixture.detectChanges();
    expect(fixture.nativeElement.getAttribute('data-map-mode')).toBe('loading');
    expect(el.querySelector('.map-placeholder')).not.toBeNull();
    http.expectOne('/api/v1/map/coverage').flush({
      bounds: SF_BOUNDS,
      minZoom: 0,
      maxZoom: 14,
      center: { lon: -122.42, lat: 37.77, zoom: 12 },
      tileUrlTemplate: '/api/v1/map/tiles/{z}/{x}/{y}',
      tileFormat: 'mvt',
      attribution: '© OpenStreetMap contributors',
    });
  });

  it('renders the local GL view inline when the destination is inside the archive bounds', async () => {
    setup(card());
    fixture.detectChanges();
    http.expectOne('/api/v1/map/coverage').flush({
      bounds: SF_BOUNDS,
      minZoom: 0,
      maxZoom: 14,
      center: { lon: -122.42, lat: 37.77, zoom: 12 },
      tileUrlTemplate: '/api/v1/map/tiles/{z}/{x}/{y}',
      tileFormat: 'mvt',
      attribution: '© OpenStreetMap contributors',
    });
    await fixture.whenStable();
    fixture.detectChanges();

    expect(fixture.nativeElement.getAttribute('data-map-mode')).toBe('local');
    expect(el.querySelector('.map-gl-wrap')).not.toBeNull();
    expect(el.querySelector('.open-large')).not.toBeNull();
    expect(el.querySelector('.coords-only')).toBeNull();
  });

  it('falls back to online raster when the archive does not cover the destination and the network is up', async () => {
    setup(card({ destination: OUT_OF_REGION_DESTINATION }));
    fixture.detectChanges();
    http.expectOne('/api/v1/map/coverage').flush({
      bounds: SF_BOUNDS,
      minZoom: 0,
      maxZoom: 14,
      center: { lon: -122.42, lat: 37.77, zoom: 12 },
      tileUrlTemplate: '/api/v1/map/tiles/{z}/{x}/{y}',
      tileFormat: 'mvt',
      attribution: '© OpenStreetMap contributors',
    });
    await fixture.whenStable();
    fixture.detectChanges();

    expect(fixture.nativeElement.getAttribute('data-map-mode')).toBe('raster');
    expect(el.querySelector('.map-gl-wrap')).not.toBeNull();
  });

  it('falls back to coordinates-only, never blank, when out of coverage and offline', async () => {
    setup(card({ destination: OUT_OF_REGION_DESTINATION }));
    fixture.detectChanges();
    window.dispatchEvent(new Event('offline'));
    http.expectOne('/api/v1/map/coverage').flush({
      bounds: SF_BOUNDS,
      minZoom: 0,
      maxZoom: 14,
      center: { lon: -122.42, lat: 37.77, zoom: 12 },
      tileUrlTemplate: '/api/v1/map/tiles/{z}/{x}/{y}',
      tileFormat: 'mvt',
      attribution: '© OpenStreetMap contributors',
    });
    await fixture.whenStable();
    fixture.detectChanges();

    expect(fixture.nativeElement.getAttribute('data-map-mode')).toBe('coords');
    expect(el.querySelector('.map-gl-wrap')).toBeNull();
    const coordsBlock = el.querySelector('.coords-only');
    expect(coordsBlock).not.toBeNull();
    expect(coordsBlock?.textContent).toContain('13.4125° N, 103.8667° E');
  });

  it('shows a compass bearing in coordinates-only mode when a current-location point is carried', async () => {
    setup(
      card({
        destination: OUT_OF_REGION_DESTINATION,
        currentLocation: { lon: 103.9, lat: 13.4125 },
      }),
    );
    fixture.detectChanges();
    window.dispatchEvent(new Event('offline'));
    http.expectOne('/api/v1/map/coverage').flush(null, { status: 404, statusText: 'Not Found' });
    await fixture.whenStable();
    fixture.detectChanges();

    expect(fixture.nativeElement.getAttribute('data-map-mode')).toBe('coords');
    expect(el.querySelector('.bearing-line')?.textContent).toContain('W from current location');
  });

  it('treats a failed coverage lookup as not-covered rather than assuming local coverage', async () => {
    setup(card());
    fixture.detectChanges();
    http
      .expectOne('/api/v1/map/coverage')
      .flush('boom', { status: 503, statusText: 'Service Unavailable' });
    await fixture.whenStable();
    fixture.detectChanges();

    // Online by default in the test browser, so a failed lookup falls to the
    // online raster fallback rather than being (wrongly) trusted as local.
    expect(fixture.nativeElement.getAttribute('data-map-mode')).toBe('raster');
  });

  it('"open large" toggles the expanded overlay and both the button and Escape close it', async () => {
    setup(card());
    fixture.detectChanges();
    http.expectOne('/api/v1/map/coverage').flush({
      bounds: SF_BOUNDS,
      minZoom: 0,
      maxZoom: 14,
      center: { lon: -122.42, lat: 37.77, zoom: 12 },
      tileUrlTemplate: '/api/v1/map/tiles/{z}/{x}/{y}',
      tileFormat: 'mvt',
      attribution: '© OpenStreetMap contributors',
    });
    await fixture.whenStable();
    fixture.detectChanges();

    const openButton = el.querySelector<HTMLButtonElement>('.open-large');
    expect(openButton).not.toBeNull();
    openButton?.click();
    fixture.detectChanges();
    expect(el.querySelector('.map-gl-wrap.expanded')).not.toBeNull();
    expect(el.querySelector('.map-overlay-close')).not.toBeNull();

    // The close affordance is never gated behind the lazy GL chunk loading.
    el.querySelector<HTMLButtonElement>('.map-overlay-close')?.click();
    fixture.detectChanges();
    expect(el.querySelector('.map-gl-wrap.expanded')).toBeNull();

    openButton?.click();
    fixture.detectChanges();
    expect(el.querySelector('.map-gl-wrap.expanded')).not.toBeNull();
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    fixture.detectChanges();
    expect(el.querySelector('.map-gl-wrap.expanded')).toBeNull();
  });

  it('renders the OSM attribution string carried by the coverage response, never hidden text', async () => {
    // The attribution is handed to the (deferred) GL view as an input, not
    // rendered by MapCard itself — this asserts the wiring reaches it rather
    // than duplicating MapGlView's own rendering.
    setup(card());
    fixture.detectChanges();
    http.expectOne('/api/v1/map/coverage').flush({
      bounds: SF_BOUNDS,
      minZoom: 0,
      maxZoom: 14,
      center: { lon: -122.42, lat: 37.77, zoom: 12 },
      tileUrlTemplate: '/api/v1/map/tiles/{z}/{x}/{y}',
      tileFormat: 'mvt',
      attribution: '© OpenStreetMap contributors — Acme Regional Extract',
    });
    await fixture.whenStable();
    fixture.detectChanges();
    expect(fixture.nativeElement.getAttribute('data-map-mode')).toBe('local');
  });

  it('renders distance and walk time as tabular-nums footer text when present', async () => {
    setup(card({ distance: '1.2 mi', walkTime: '24 min' }));
    fixture.detectChanges();
    http.expectOne('/api/v1/map/coverage').flush(null, { status: 404, statusText: 'Not Found' });
    await fixture.whenStable();
    fixture.detectChanges();

    const footer = el.querySelector('.map-footer');
    expect(footer?.textContent).toContain('1.2 mi');
    expect(footer?.textContent).toContain('24 min');
    const style = getComputedStyle(footer as Element);
    expect(style.fontVariantNumeric).toContain('tabular-nums');
  });
});

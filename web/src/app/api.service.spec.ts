import { TestBed } from '@angular/core/testing';
import { provideZonelessChangeDetection } from '@angular/core';
import { provideHttpClient, withXhr } from '@angular/common/http';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { ApiService } from './api.service';

describe('ApiService', () => {
  let api: ApiService;
  let http: HttpTestingController;

  beforeEach(() => {
    localStorage.clear();
    TestBed.configureTestingModule({
      providers: [
        provideZonelessChangeDetection(),
        provideHttpClient(withXhr()),
        provideHttpClientTesting(),
      ],
    });
    api = TestBed.inject(ApiService);
    http = TestBed.inject(HttpTestingController);
  });

  afterEach(() => http.verify());

  it('stores the device token after pairing and sends it as a bearer header', async () => {
    const paired = api.pair('123-456', 'web-shell');
    const pairRequest = http.expectOne('/api/v1/auth/pair');
    expect(pairRequest.request.body).toEqual({
      pairingCode: '123-456',
      deviceName: 'web-shell',
    });
    pairRequest.flush({
      deviceId: '01ARZ3NDEKTSV4RRFFQ69G5FAV',
      deviceToken: 'a'.repeat(64),
      scopes: ['ui'],
    });
    await paired;
    expect(api.hasToken()).toBeTrue();

    const listing = api.listSessions();
    const listRequest = http.expectOne('/api/v1/sessions');
    expect(listRequest.request.headers.get('Authorization')).toBe(`Bearer ${'a'.repeat(64)}`);
    listRequest.flush({ sessions: [] });
    await listing;
  });

  it('sends the idempotency key on session create', async () => {
    localStorage.setItem('jarvis.deviceToken', 't');
    const creating = api.createSession('plans', 'key-1');
    const request = http.expectOne('/api/v1/sessions');
    expect(request.request.headers.get('Idempotency-Key')).toBe('key-1');
    expect(request.request.body).toEqual({ title: 'plans' });
    request.flush({
      id: '01ARZ3NDEKTSV4RRFFQ69G5FAV',
      status: 'active',
      title: 'plans',
      createdAt: '2026-07-19T00:00:00Z',
      updatedAt: '2026-07-19T00:00:00Z',
    });
    await creating;
  });

  describe('getMapCoverage (F3b.5, docs/12 §3)', () => {
    it('returns the coverage response when an archive is configured', async () => {
      const coverage = api.getMapCoverage();
      const request = http.expectOne('/api/v1/map/coverage');
      request.flush({
        bounds: { minLon: -1, minLat: -1, maxLon: 1, maxLat: 1 },
        minZoom: 0,
        maxZoom: 14,
        center: { lon: 0, lat: 0, zoom: 10 },
        tileUrlTemplate: '/api/v1/map/tiles/{z}/{x}/{y}',
        tileFormat: 'mvt',
        attribution: '© OpenStreetMap contributors',
      });
      expect((await coverage)?.tileFormat).toBe('mvt');
    });

    it('resolves to null on a 404 — no archive configured is absent, not an error', async () => {
      const coverage = api.getMapCoverage();
      const request = http.expectOne('/api/v1/map/coverage');
      request.flush('not found', { status: 404, statusText: 'Not Found' });
      expect(await coverage).toBeNull();
    });

    it('rethrows any other failure — a 5xx is not "no archive"', async () => {
      const coverage = api.getMapCoverage();
      const request = http.expectOne('/api/v1/map/coverage');
      request.flush('boom', { status: 503, statusText: 'Service Unavailable' });
      await expectAsync(coverage).toBeRejected();
    });
  });
});

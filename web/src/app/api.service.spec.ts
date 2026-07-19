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
});

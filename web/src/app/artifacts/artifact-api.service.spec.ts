import { TestBed } from '@angular/core/testing';
import { provideZonelessChangeDetection } from '@angular/core';
import { provideHttpClient, withXhr } from '@angular/common/http';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { ArtifactApiService } from './artifact-api.service';

describe('ArtifactApiService', () => {
  let api: ArtifactApiService;
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
    api = TestBed.inject(ArtifactApiService);
    http = TestBed.inject(HttpTestingController);
  });

  afterEach(() => http.verify());

  it('lists versions with the bearer token attached', async () => {
    localStorage.setItem('jarvis.deviceToken', 'tok-1');
    const pending = api.getVersions('01ARZ3NDEKTSV4RRFFQ69G5FAV');
    const req = http.expectOne('/api/v1/artifacts/01ARZ3NDEKTSV4RRFFQ69G5FAV/versions');
    expect(req.request.headers.get('Authorization')).toBe('Bearer tok-1');
    req.flush({ artifactId: '01ARZ3NDEKTSV4RRFFQ69G5FAV', versions: [] });
    await pending;
  });

  it('fetches a version blob as text', async () => {
    const pending = api.getBlobText('a1', 2);
    const req = http.expectOne('/api/v1/artifacts/a1/versions/2/blob');
    expect(req.request.responseType).toBe('text');
    req.flush('# hello');
    expect(await pending).toBe('# hello');
  });

  it('fetches a version blob as a Blob', async () => {
    const pending = api.getBlobBlob('a1', 1);
    const req = http.expectOne('/api/v1/artifacts/a1/versions/1/blob');
    expect(req.request.responseType).toBe('blob');
    req.flush(new Blob(['bytes']));
    const blob = await pending;
    expect(blob instanceof Blob).toBeTrue();
  });
});

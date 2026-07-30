import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { provideHttpClient, withXhr } from '@angular/common/http';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { ActivatedRoute, convertToParamMap } from '@angular/router';
import type { ArtifactManifestDto, ArtifactVersionsResponse } from '../../generated/api-types';
import { ArtifactCanvas } from './artifact-canvas';

function manifest(overrides: Partial<ArtifactManifestDto> = {}): ArtifactManifestDto {
  return {
    id: 'art-1',
    version: 1,
    createdByRun: '01ARZ3NDEKTSV4RRFFQ69G5FAV',
    sha256: 'a'.repeat(64),
    mediaType: 'text/markdown',
    kind: 'markdown_html',
    renderer: 'markdown_html@1',
    sources: [],
    sensitivity: 'normal',
    build: { network: 'disabled' },
    capabilities: [],
    ...overrides,
  };
}

describe('ArtifactCanvas', () => {
  let fixture: ComponentFixture<ArtifactCanvas>;
  let http: HttpTestingController;
  let el: HTMLElement;

  function setup(id = 'art-1'): void {
    TestBed.configureTestingModule({
      providers: [
        provideZonelessChangeDetection(),
        provideHttpClient(withXhr()),
        provideHttpClientTesting(),
        {
          provide: ActivatedRoute,
          useValue: { snapshot: { paramMap: convertToParamMap({ id }) } },
        },
      ],
    });
    http = TestBed.inject(HttpTestingController);
    fixture = TestBed.createComponent(ArtifactCanvas);
    el = fixture.nativeElement as HTMLElement;
  }

  afterEach(() => http.verify());

  it('loads the latest version and shows content + provenance (exit evidence #1: reopen after restart)', async () => {
    setup();
    fixture.detectChanges();
    await fixture.whenStable();

    const versionsReq = http.expectOne('/api/v1/artifacts/art-1/versions');
    const resp: ArtifactVersionsResponse = {
      artifactId: 'art-1',
      versions: [
        manifest({ version: 1 }),
        manifest({
          version: 2,
          sensitivity: 'sensitive',
          sources: [{ kind: 'web', reference: 'https://example.com/page' }],
        }),
      ],
    };
    versionsReq.flush(resp);
    await fixture.whenStable();

    const blobReq = http.expectOne('/api/v1/artifacts/art-1/versions/2/blob');
    expect(blobReq.request.responseType).toBe('text');
    blobReq.flush('# Reopened\n\nStill here after restart.');
    await fixture.whenStable();
    fixture.detectChanges();

    expect(el.querySelector('app-markdown-renderer')?.textContent).toContain('Reopened');
    // Provenance: sensitivity, source with attribution, and the version switcher.
    expect(el.querySelector('.sensitivity')?.textContent).toContain('sensitive');
    const sourceLink = el.querySelector('.sources a') as HTMLAnchorElement;
    expect(sourceLink.href).toBe('https://example.com/page');
    expect(el.querySelectorAll('.version-switcher button').length).toBe(2);
    expect(el.querySelector('.version-switcher button.active')?.textContent).toContain('v2');
  });

  it('re-fetches the blob when an older version is selected', async () => {
    setup();
    fixture.detectChanges();
    await fixture.whenStable();
    http
      .expectOne('/api/v1/artifacts/art-1/versions')
      .flush({ artifactId: 'art-1', versions: [manifest({ version: 1 }), manifest({ version: 2 })] });
    await fixture.whenStable();
    http.expectOne('/api/v1/artifacts/art-1/versions/2/blob').flush('latest text');
    await fixture.whenStable();
    fixture.detectChanges();

    const buttons = Array.from(el.querySelectorAll('.version-switcher button')) as HTMLButtonElement[];
    buttons[0].click(); // v1
    await fixture.whenStable();
    http.expectOne('/api/v1/artifacts/art-1/versions/1/blob').flush('older text');
    await fixture.whenStable();
    fixture.detectChanges();

    expect(el.querySelector('app-markdown-renderer')?.textContent).toContain('older text');
  });

  it('shows an explicit not-found message for an unknown artifact id, not a blank panel', async () => {
    setup('missing');
    fixture.detectChanges();
    await fixture.whenStable();
    http
      .expectOne('/api/v1/artifacts/missing/versions')
      .flush({ code: 'resource.not_found', title: 'not found', status: 404, type: 'about:blank' }, { status: 404, statusText: 'Not Found' });
    await fixture.whenStable();
    fixture.detectChanges();

    expect(el.querySelector('.error')?.textContent).toContain('not found');
  });

  it('never fetches blob bytes for a bundle artifact and shows the unsupported message', async () => {
    setup();
    fixture.detectChanges();
    await fixture.whenStable();
    http
      .expectOne('/api/v1/artifacts/art-1/versions')
      .flush({ artifactId: 'art-1', versions: [manifest({ version: 1, kind: 'bundle' })] });
    await fixture.whenStable();
    // The bundle path resolves through an extra microtask hop (loadVersions
    // awaits selectVersion, which returns immediately for `bundle`) with no
    // further HTTP round-trip to anchor a second `whenStable` wait on — give
    // it one more turn before asserting `loading` has settled to false.
    await fixture.whenStable();
    fixture.detectChanges();

    // http.verify() in afterEach fails the test if a blob request was made.
    expect(el.querySelector('app-unsupported-renderer')).not.toBeNull();
    expect(el.textContent).toContain('sandbox');
  });

  it('fetches an image version as a Blob, not text', async () => {
    setup();
    fixture.detectChanges();
    await fixture.whenStable();
    http
      .expectOne('/api/v1/artifacts/art-1/versions')
      .flush({
        artifactId: 'art-1',
        versions: [manifest({ version: 1, kind: 'image', mediaType: 'image/png' })],
      });
    await fixture.whenStable();
    const blobReq = http.expectOne('/api/v1/artifacts/art-1/versions/1/blob');
    expect(blobReq.request.responseType).toBe('blob');
    blobReq.flush(new Blob(['fake-png-bytes'], { type: 'image/png' }));
    await fixture.whenStable();
    fixture.detectChanges();

    expect(el.querySelector('app-image-renderer')).not.toBeNull();
  });
});

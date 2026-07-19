import { provideZonelessChangeDetection } from '@angular/core';
import { TestBed } from '@angular/core/testing';
import { provideHttpClient, withXhr } from '@angular/common/http';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { App } from './app';

describe('App', () => {
  let http: HttpTestingController;

  beforeEach(async () => {
    localStorage.clear();
    await TestBed.configureTestingModule({
      imports: [App],
      providers: [
        provideZonelessChangeDetection(),
        provideHttpClient(withXhr()),
        provideHttpClientTesting(),
      ],
    }).compileComponents();
    http = TestBed.inject(HttpTestingController);
  });

  afterEach(() => http.verify());

  it('renders health from the daemon, typed by the generated contract', async () => {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    await fixture.whenStable();

    http.expectOne('/api/v1/diagnostics/health').flush({
      status: 'ok',
      version: '0.1.0',
      adapters: { database: { state: 'up' } },
    });
    await fixture.whenStable();
    fixture.detectChanges();

    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.querySelector('h1')?.textContent).toContain('Jarvis');
    expect(compiled.querySelector('.status')?.textContent).toContain('ok');
    expect(compiled.querySelector('.status')?.textContent).toContain('database: up');
  });

  it('offers pairing while the window is open and hides sessions until paired', async () => {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    await fixture.whenStable();

    http.expectOne('/api/v1/diagnostics/health').flush({
      status: 'ok',
      version: '0.1.0',
      adapters: {},
      pairingCode: '123-456',
    });
    await fixture.whenStable();
    fixture.detectChanges();

    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.querySelector('button')?.textContent).toContain('123-456');
    expect(compiled.querySelector('[aria-label="sessions"]')).toBeNull();
  });

  it('reports the daemon unreachable instead of failing silently', async () => {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    await fixture.whenStable();

    http.expectOne('/api/v1/diagnostics/health').error(new ProgressEvent('error'), { status: 0 });
    await fixture.whenStable();
    fixture.detectChanges();

    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.querySelector('.error')?.textContent).toContain('not reachable');
  });
});

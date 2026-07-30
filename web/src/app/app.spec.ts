import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { provideHttpClient, withXhr } from '@angular/common/http';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { App } from './app';
import { HudStateService } from './hud/hud-state.service';

describe('App', () => {
  let http: HttpTestingController;
  let hud: HudStateService;

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
    hud = TestBed.inject(HudStateService);
    hud.setOpsOpen(false);
  });

  afterEach(() => {
    hud.stopReveal();
    http.verify();
  });

  /** Boot the shell and answer the health probe. */
  async function boot(health: Record<string, unknown>): Promise<ComponentFixture<App>> {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    await fixture.whenStable();
    http.expectOne('/api/v1/diagnostics/health').flush(health);
    await fixture.whenStable();
    fixture.detectChanges();
    return fixture;
  }

  /** The ops layer is where the M0/M1 operator surfaces live after the pivot. */
  async function openOps(fixture: ComponentFixture<App>): Promise<HTMLElement> {
    hud.setOpsOpen(true);
    await fixture.whenStable();
    fixture.detectChanges();
    return fixture.nativeElement as HTMLElement;
  }

  it('shows the HUD face by default, with the operator surfaces behind it', async () => {
    // docs/12 §1: the front face is the HUD. Health/sessions are not gone —
    // they are one keystroke away.
    const fixture = await boot({ status: 'ok', version: '0.1.0', adapters: {} });
    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.querySelector('app-hud')).not.toBeNull();
    expect(compiled.querySelector('.ops-layer')).toBeNull();

    const ops = await openOps(fixture);
    expect(ops.querySelector('app-hud')).toBeNull();
    expect(ops.querySelector('.ops-layer')).not.toBeNull();
  });

  it('toggles the ops layer with Ctrl+. and closes it with Escape', async () => {
    const fixture = await boot({ status: 'ok', version: '0.1.0', adapters: {} });

    document.dispatchEvent(new KeyboardEvent('keydown', { key: '.', ctrlKey: true }));
    await fixture.whenStable();
    fixture.detectChanges();
    expect(hud.opsOpen()).toBe(true);
    expect((fixture.nativeElement as HTMLElement).querySelector('.ops-layer')).not.toBeNull();

    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    await fixture.whenStable();
    fixture.detectChanges();
    expect(hud.opsOpen()).toBe(false);
  });

  it('renders health from the daemon, typed by the generated contract', async () => {
    const fixture = await boot({
      status: 'ok',
      version: '0.1.0',
      adapters: { database: { state: 'up' } },
    });
    const ops = await openOps(fixture);

    expect(ops.querySelector('h1')?.textContent).toContain('Jarvis');
    expect(ops.querySelector('.status')?.textContent).toContain('ok');
    expect(ops.querySelector('.status')?.textContent).toContain('database: up');
  });

  it('offers pairing while the window is open and hides sessions until paired', async () => {
    const fixture = await boot({
      status: 'ok',
      version: '0.1.0',
      adapters: {},
      pairingCode: '123-456',
    });
    const ops = await openOps(fixture);

    expect(ops.querySelector('.shell-ui button')?.textContent).toContain('123-456');
    expect(ops.querySelector('[aria-label="sessions"]')).toBeNull();
  });

  it('reports the daemon unreachable instead of failing silently, and says so on the orb', async () => {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    await fixture.whenStable();
    http.expectOne('/api/v1/diagnostics/health').error(new ProgressEvent('error'), { status: 0 });
    await fixture.whenStable();
    fixture.detectChanges();

    // The HUD state language carries the failure too — an unreachable daemon is
    // not just a line of text in a layer nobody has open (docs/12 §2.1).
    expect(hud.presence()).toBe('error');

    const ops = await openOps(fixture);
    expect(ops.querySelector('.error')?.textContent).toContain('not reachable');
  });
});

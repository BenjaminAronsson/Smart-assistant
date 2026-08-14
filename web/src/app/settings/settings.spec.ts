import { ComponentFixture, TestBed } from '@angular/core/testing';
import { provideZonelessChangeDetection } from '@angular/core';
import { Settings } from './settings';
import { ApiService } from '../api.service';
import type { AutomationDto, DeviceDto } from '../../generated/api-types';

function device(over: Partial<DeviceDto> = {}): DeviceDto {
  return {
    deviceId: '01ARZ3NDEKTSV4RRFFQ69G5FAV',
    name: 'kitchen screen',
    deviceClass: 'room-node',
    executesTools: false,
    scopes: ['display-agent', 'voice-capture'],
    createdAt: '2026-08-13T10:00:00Z',
    lastSeenAt: '2026-08-14T08:00:00Z',
    ...over,
  } as DeviceDto;
}

function automation(over: Partial<AutomationDto> = {}): AutomationDto {
  return {
    id: '01ARZ3NDEKTSV4RRFFQ69G5FB1',
    name: 'evening lights',
    trigger: { type: 'daily_at', minutesSinceMidnight: 420 },
    toolId: 'home.set_light',
    arguments: {},
    enabled: true,
    createdByDeviceId: '01ARZ3NDEKTSV4RRFFQ69G5FAV',
    createdAt: '2026-08-13T10:00:00Z',
    ...over,
  } as AutomationDto;
}

class FakeApi {
  devices: DeviceDto[] = [device()];
  automations: AutomationDto[] = [automation()];
  revoked: { id: string; reason: string }[] = [];
  toggled: { id: string; enabled: boolean }[] = [];
  windowsOpened = 0;

  listDevices() {
    return Promise.resolve({ devices: this.devices });
  }
  listAutomations() {
    return Promise.resolve({ automations: this.automations });
  }
  openPairingWindow() {
    this.windowsOpened += 1;
    return Promise.resolve({ pairingCode: '123-456', expiresAt: '2026-08-14T09:00:00Z' });
  }
  revokeDevice(id: string, reason: string) {
    this.revoked.push({ id, reason });
    this.devices = this.devices.map((d) =>
      d.deviceId === id ? { ...d, revokedAt: '2026-08-14T08:30:00Z' } : d,
    );
    return Promise.resolve();
  }
  setAutomationEnabled(id: string, enabled: boolean) {
    this.toggled.push({ id, enabled });
    this.automations = this.automations.map((a) => (a.id === id ? { ...a, enabled } : a));
    return Promise.resolve();
  }
  automationHistory() {
    return Promise.resolve({
      executions: [
        { occurredAt: '2026-08-14T07:00:00Z', outcome: 'denied', detail: 'missing scope' },
      ],
    });
  }
}

describe('Settings', () => {
  let fixture: ComponentFixture<Settings>;
  let component: Settings;
  let api: FakeApi;

  beforeEach(async () => {
    api = new FakeApi();
    await TestBed.configureTestingModule({
      imports: [Settings],
      providers: [
        provideZonelessChangeDetection(),
        { provide: ApiService, useValue: api },
      ],
    }).compileComponents();

    fixture = TestBed.createComponent(Settings);
    component = fixture.componentInstance;
    await component.ngOnInit();
    fixture.detectChanges();
  });

  it('renders the M7 device DTOs, including what a node may not do', () => {
    const text = (fixture.nativeElement as HTMLElement).textContent ?? '';
    expect(text).toContain('kitchen screen');
    expect(text).toContain('room-node');
    // The class decides authority and the list says so, rather than inferring.
    expect(text).toContain('presents and listens only');
  });

  it('asks once before revoking, and takes effect visibly', async () => {
    component.askToRevoke('01ARZ3NDEKTSV4RRFFQ69G5FAV');
    fixture.detectChanges();
    expect((fixture.nativeElement as HTMLElement).textContent).toContain('Yes, revoke');
    // Asking is not doing.
    expect(api.revoked.length).toBe(0);

    await component.confirmRevoke('01ARZ3NDEKTSV4RRFFQ69G5FAV');
    fixture.detectChanges();

    expect(api.revoked.length).toBe(1);
    // Visible: the device is still listed, marked revoked, rather than gone —
    // a disappearing row looks like the revoke failed.
    expect(component.devices()[0].revokedAt).toBeTruthy();
    expect((fixture.nativeElement as HTMLElement).textContent).toContain('Revoked');
  });

  it('lets the owner back out of a revoke', () => {
    component.askToRevoke('01ARZ3NDEKTSV4RRFFQ69G5FAV');
    component.cancelRevoke();
    fixture.detectChanges();
    expect(component.confirmingRevoke()).toBeNull();
    expect(api.revoked.length).toBe(0);
  });

  it('shows a pairing code and clears it once the node appears, without a reload', async () => {
    await component.pairNode();
    fixture.detectChanges();
    expect((fixture.nativeElement as HTMLElement).textContent).toContain('123-456');

    // The node pairs while the owner is standing at it.
    api.devices = [...api.devices, device({ deviceId: '01ARZ3NDEKTSV4RRFFQ69G5FB2', name: 'hall' })];
    await component.checkForNewDevice();
    fixture.detectChanges();

    expect(component.devices().length).toBe(2);
    // The window is single-use and now spent; a dead code on screen is worse
    // than none.
    expect(component.pairingWindow()).toBeNull();
    expect((fixture.nativeElement as HTMLElement).textContent).toContain('hall');
  });

  it('toggles an automation and reflects the new state', async () => {
    await component.toggleAutomation(component.automations()[0]);
    fixture.detectChanges();
    expect(api.toggled).toEqual([{ id: '01ARZ3NDEKTSV4RRFFQ69G5FB1', enabled: false }]);
    expect(component.automations()[0].enabled).toBe(false);
  });

  it('shows a refusal in the history as a refusal', async () => {
    await component.toggleHistory(component.automations()[0]);
    fixture.detectChanges();
    const text = (fixture.nativeElement as HTMLElement).textContent ?? '';
    // Not softened: "refused" and "ran and did nothing" are otherwise
    // indistinguishable from the sofa.
    expect(text).toContain('Refused');
    expect(text).toContain('missing scope');
  });

  it('renders a readable trigger rather than raw JSON', () => {
    expect(component.triggerLabel(component.automations()[0])).toBe('Every day at 07:00');
    expect(
      component.triggerLabel(
        automation({ trigger: { type: 'ha_state', entityId: 'person.owner', state: 'home' } }),
      ),
    ).toBe('When person.owner becomes home');
  });

  it('is operable by keyboard alone (NFR-11)', () => {
    const element = fixture.nativeElement as HTMLElement;
    const buttons = Array.from(element.querySelectorAll('button'));
    expect(buttons.length).toBeGreaterThan(0);
    for (const button of buttons) {
      // Real buttons, in tab order — nothing reachable only by pointer, and no
      // click handler bolted to a div.
      expect(button.tagName).toBe('BUTTON');
      expect(button.getAttribute('tabindex')).not.toBe('-1');
    }
    // Sections are labelled so a screen reader can navigate by heading.
    expect(element.querySelectorAll('section[aria-labelledby]').length).toBeGreaterThan(1);
    // Failures are announced, not just coloured.
    expect(element.querySelector('[role="alert"][aria-live]')).toBeTruthy();
  });

  it('never shows a raw transport error to the owner', async () => {
    api.listDevices = () => Promise.reject(new Error('ECONNREFUSED 127.0.0.1:8741'));
    await component.refresh();
    fixture.detectChanges();
    expect(component.error()).toBe('Could not load settings. Is the daemon running?');
    expect((fixture.nativeElement as HTMLElement).textContent).not.toContain('ECONNREFUSED');
  });
});

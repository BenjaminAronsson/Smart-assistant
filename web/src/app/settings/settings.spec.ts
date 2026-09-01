import { ComponentFixture, TestBed } from '@angular/core/testing';
import { provideZonelessChangeDetection } from '@angular/core';
import { HttpErrorResponse } from '@angular/common/http';
import { Settings, describeFailure } from './settings';
import { ApiService } from '../api.service';
import type { PolicyViewDto } from '../../generated/api-types';
import type {
  AutomationDto,
  DeviceDto,
  UpdateVoiceSettingsRequest,
  VoiceSettingsDto,
} from '../../generated/api-types';

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

function voiceSettings(over: Partial<VoiceSettingsDto> = {}): VoiceSettingsDto {
  return {
    // A word with no provisioned model. The shipped default is `hey jarvis`,
    // which has one — but an owner can still configure a word that does not,
    // and the surface must say so rather than look healthy.
    wakeWord: 'andy',
    availableWakeWords: ['alexa', 'hey jarvis'],
    wakeWordWarning: 'no wake-word model is provisioned for "andy"',
    elevenlabs: {
      configured: true,
      enabled: false,
      spentCharacters: 12_480,
      characterBudget: 100_000,
      period: '2026-08',
      localFallback: 'wyoming-tts',
    },
    ...over,
  } as VoiceSettingsDto;
}

class FakeApi {
  devices: DeviceDto[] = [device()];
  voice: VoiceSettingsDto = voiceSettings();
  voicePatches: UpdateVoiceSettingsRequest[] = [];
  /** Set to make the daemon refuse, as it does when unconfigured. */
  refuseVoice = false;

  getVoiceSettings() {
    return Promise.resolve(this.voice);
  }
  updateVoiceSettings(patch: UpdateVoiceSettingsRequest) {
    this.voicePatches.push(patch);
    if (this.refuseVoice) return Promise.reject(new Error('refused'));
    const word = patch.wakeWord;
    if (typeof word === 'string') {
      this.voice = { ...this.voice, wakeWord: word, wakeWordWarning: undefined };
    }
    const enabled = patch.elevenlabsEnabled;
    if (typeof enabled === 'boolean') {
      this.voice = {
        ...this.voice,
        elevenlabs: { ...this.voice.elevenlabs, enabled },
      };
    }
    return Promise.resolve(this.voice);
  }
  automations: AutomationDto[] = [automation()];
  revoked: { id: string; reason: string }[] = [];
  toggled: { id: string; enabled: boolean }[] = [];
  windowsOpened = 0;

  /**
   * The policy view (F10.5). Shaped like the daemon's real response, including
   * an outcome for every class the component renders — a double that returned
   * fewer would let the component pass here while showing "unknown" against a
   * real daemon, which is this project's most expensive recurring bug.
   */
  policy: PolicyViewDto = {
    tools: [
      {
        toolId: 'example.light',
        risk: 'R1',
        reversible: true,
        requiresUserPresence: false,
        egress: 'local',
        speechSensitivity: 'normal',
        requiredScopes: ['home:control'],
        outcomes: [
          { deviceClass: 'owner-ui', outcome: 'auto' },
          { deviceClass: 'room-node', outcome: 'denied', reason: 'missing_scope:home:control' },
          { deviceClass: 'voice-node', outcome: 'denied', reason: 'missing_scope:home:control' },
          { deviceClass: 'display-node', outcome: 'denied', reason: 'missing_scope:home:control' },
        ],
      },
    ],
  };

  getPolicy() {
    return Promise.resolve(this.policy);
  }

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
    // The intent is the assertion, not the exact copy: whatever the owner is
    // told, it is never the transport's own words.
    expect(component.error()).toBe('Could not load settings.');
    expect((fixture.nativeElement as HTMLElement).textContent).not.toContain('ECONNREFUSED');
    expect((fixture.nativeElement as HTMLElement).textContent).not.toContain('127.0.0.1');
  });

  describe('voice', () => {
    it('shows the wake word, the spend and the fallback', () => {
      const text = fixture.nativeElement.textContent as string;
      expect(text).toContain('12,480');
      expect(text).toContain('100,000');
      expect(text).toContain('wyoming-tts');
      expect(component.spendPercent()).toBe(12);
    });

    // The "Andy" case: openWakeWord publishes no model for it, so a node
    // configured that way answers to nothing while looking perfectly healthy.
    it('names a wake word that has no model rather than hiding it', () => {
      const text = fixture.nativeElement.textContent as string;
      expect(text).toContain('no wake-word model is provisioned');
    });

    it('changes the wake word to one that has a model', async () => {
      await component.setWakeWord('hey jarvis');
      expect(api.voicePatches).toEqual([{ wakeWord: 'hey jarvis' }]);
      expect(component.voice()?.wakeWord).toBe('hey jarvis');
    });

    // Granting consent is the moment the house's voice starts leaving the
    // house (ADR-033 §2), so it is asked about once rather than toggled.
    it('asks before granting consent, and sends nothing until confirmed', async () => {
      await component.toggleElevenLabs();
      expect(component.confirmingElevenLabs()).toBe(true);
      expect(api.voicePatches).toEqual([]);

      await component.confirmElevenLabs();
      expect(component.confirmingElevenLabs()).toBe(false);
      expect(api.voicePatches).toEqual([{ elevenlabsEnabled: true }]);
      expect(component.voice()?.elevenlabs.enabled).toBe(true);
    });

    it('cancelling leaves it off and sends nothing', async () => {
      await component.toggleElevenLabs();
      component.cancelElevenLabs();
      expect(component.confirmingElevenLabs()).toBe(false);
      expect(api.voicePatches).toEqual([]);
      expect(component.voice()?.elevenlabs.enabled).toBe(false);
    });

    // Deliberately asymmetric: withdrawing consent is the owner protecting
    // themselves, and a confirmation there would be the interface arguing.
    it('withdraws consent immediately, without asking', async () => {
      await component.toggleElevenLabs();
      await component.confirmElevenLabs();
      api.voicePatches = [];

      await component.toggleElevenLabs();
      expect(component.confirmingElevenLabs()).toBe(false);
      expect(api.voicePatches).toEqual([{ elevenlabsEnabled: false }]);
      expect(component.voice()?.elevenlabs.enabled).toBe(false);
    });

    it('reports a refusal without echoing a raw error', async () => {
      api.refuseVoice = true;
      await component.toggleElevenLabs();
      await component.confirmElevenLabs();
      expect(component.error()).toContain('local voice');
      expect(component.error()).not.toContain('refused');
    });

    it('reads the period as a month', () => {
      expect(component.periodLabel('2026-08')).toContain('2026');
      expect(component.periodLabel('nonsense')).toBe('nonsense');
    });
  });


  describe('failure messages', () => {
    // The most common first-run state: the shell is open on a machine that has
    // never paired. It used to read "Is the daemon running?" while the daemon
    // was running perfectly well and answering 401 — sending the owner to debug
    // the wrong thing on their very first visit.
    it('tells an unpaired browser to pair, not to check the daemon', () => {
      const message = describeFailure(new HttpErrorResponse({ status: 401 }));
      expect(message).toContain('not paired');
      expect(message).not.toContain('Is it running');
    });

    it('distinguishes a forbidden device from an unpaired one', () => {
      expect(describeFailure(new HttpErrorResponse({ status: 403 }))).toContain(
        'not allowed',
      );
    });

    // Angular reports a request that never reached anybody as status 0.
    it('says the daemon is unreachable only when it actually is', () => {
      expect(describeFailure(new HttpErrorResponse({ status: 0 }))).toContain(
        'Could not reach the daemon',
      );
    });

    it('never echoes a raw transport string (docs/06 §5)', () => {
      const raw = 'ECONNREFUSED 127.0.0.1:8741 secret-token-abc';
      const message = describeFailure(
        new HttpErrorResponse({ status: 500, statusText: raw, error: raw }),
      );
      expect(message).not.toContain('ECONNREFUSED');
      expect(message).not.toContain('secret-token-abc');
    });
  });


  describe('policy view (F10.5)', () => {
    /**
     * The UI half of "the rendered policy matches the engine's decisions". The
     * daemon obtains each outcome from `policy::evaluate`; the component must
     * *render what it was sent* rather than re-deriving anything from the risk
     * tier, which would be a second copy of the rules free to drift.
     */
    it('renders the outcome the server sent, per device class', async () => {
      const fixture = TestBed.createComponent(Settings);
      await fixture.componentInstance.ngOnInit();
      fixture.detectChanges();

      const text = (fixture.nativeElement as HTMLElement).textContent ?? '';
      expect(text).toContain('example.light');
      expect(text).toContain('R1');
      // owner-ui was sent `auto`; the nodes were sent a scope denial.
      expect(text).toContain('runs');
      expect(text).toContain('not allowed');
    });

    /**
     * An R1 tool "runs" for the owner — but if the server says a class is
     * denied, the page must say denied even though the tier alone would suggest
     * otherwise. This is the drift the feature warns about, checked from the UI
     * side: tier is not the answer, the engine's outcome is.
     */
    it('trusts the outcome over the risk tier', async () => {
      const fixture = TestBed.createComponent(Settings);
      const component = fixture.componentInstance;
      await component.ngOnInit();

      const tool = component.policy()!.tools[0];
      expect(tool.risk).toBe('R1');
      expect(component.outcomeFor(tool, 'owner-ui')).toBe('runs');
      expect(component.outcomeFor(tool, 'room-node')).toContain('not allowed');
      expect(component.outcomeFor(tool, 'room-node')).toContain('home:control');
    });

    it('says so when nothing is registered rather than looking empty', async () => {
      api.policy = { tools: [] };
      const fixture = TestBed.createComponent(Settings);
      await fixture.componentInstance.ngOnInit();
      fixture.detectChanges();

      const text = (fixture.nativeElement as HTMLElement).textContent ?? '';
      expect(text).toContain('No tools are registered');
    });
  });
});
import { HttpErrorResponse } from '@angular/common/http';
import { Component, type OnInit, computed, inject, signal } from '@angular/core';
import type {
  AutomationDto,
  AutomationExecutionDto,
  DeviceDto,
  PairingWindowDto,
  PolicyViewDto,
  VoiceSettingsDto,
} from '../../generated/api-types';
import { ApiService } from '../api.service';

/**
 * The settings surface (F8.8, FR-19/FR-17, docs/12).
 *
 * Until now device management was API-only and pairing a node meant `curl`.
 * This is what makes the house administrable by the person who lives in it.
 *
 * Two rules shape it:
 *
 * 1. **Nothing here infers authority.** Classes, scopes and `executesTools`
 *    are rendered from what the server sent (docs/05 §6.3) — the shell never
 *    decides what a device may do, it reports what it was told.
 * 2. **Keyboard-first** (NFR-11). Every action is a real `<button>` in tab
 *    order with a visible focus ring; nothing is reachable only by pointer.
 *    The revoke confirmation is inline rather than a modal for the same
 *    reason — a modal that traps focus badly is worse than no modal.
 */
@Component({
  selector: 'app-settings',
  standalone: true,
  templateUrl: './settings.html',
  styleUrl: './settings.scss',
})
export class Settings implements OnInit {
  private readonly api = inject(ApiService);

  readonly devices = signal<DeviceDto[]>([]);
  readonly automations = signal<AutomationDto[]>([]);
  readonly history = signal<Record<string, AutomationExecutionDto[]>>({});
  readonly pairingWindow = signal<PairingWindowDto | null>(null);
  readonly voice = signal<VoiceSettingsDto | null>(null);
  readonly policy = signal<PolicyViewDto | null>(null);
  /** Asked once before consent is granted, never before it is withdrawn. */
  readonly confirmingElevenLabs = signal(false);
  readonly error = signal<string | null>(null);
  readonly busy = signal(false);

  /** Which device the owner is being asked to confirm revoking. */
  readonly confirmingRevoke = signal<string | null>(null);
  /** Which automation's history is expanded. */
  readonly expanded = signal<string | null>(null);

  /**
   * Active devices first, then revoked. A revoked device stays visible: it is
   * the answer to "did I actually turn that thing off?", and hiding it makes
   * the list look like the revoke silently failed.
   */
  readonly sortedDevices = computed(() =>
    [...this.devices()].sort((a, b) => {
      const revoked = Number(Boolean(a.revokedAt)) - Number(Boolean(b.revokedAt));
      return revoked !== 0 ? revoked : a.name.localeCompare(b.name);
    }),
  );

  /**
   * The outcome for one device class, for display.
   *
   * Reads what the server sent; it never computes an outcome. The server got
   * each one from `policy::evaluate` itself, and re-deriving anything here
   * would reintroduce exactly the drift F10.5 exists to prevent — a UI that
   * describes different rules than the engine enforces is worse than none.
   */
  outcomeFor(tool: PolicyViewDto['tools'][number], deviceClass: string): string {
    const found = tool.outcomes.find((o) => o.deviceClass === deviceClass);
    if (!found) return 'unknown';
    switch (found.outcome) {
      case 'auto':
        return 'runs';
      case 'needs_approval':
        return 'asks first';
      default:
        // Denials carry the engine's own reason; a missing scope is the common
        // one and is worth naming, because "denied" alone reads as a fault.
        return 'reason' in found && found.reason?.startsWith('missing_scope:')
          ? `not allowed (${found.reason.slice('missing_scope:'.length)})`
          : 'not allowed';
    }
  }

  /** Device classes in the order the view lists them, owner first. */
  readonly policyClasses = ['owner-ui', 'room-node', 'voice-node', 'display-node'];

  async ngOnInit(): Promise<void> {
    await this.refresh();
  }

  async refresh(): Promise<void> {
    this.busy.set(true);
    this.error.set(null);
    try {
      const [devices, automations, voice, policy] = await Promise.all([
        this.api.listDevices(),
        this.api.listAutomations(),
        this.api.getVoiceSettings(),
        this.api.getPolicy(),
      ]);
      this.devices.set(devices.devices);
      this.automations.set(automations.automations);
      this.voice.set(voice);
      this.policy.set(policy);
    } catch (failure) {
      // Never the raw error (docs/06 §5) — but *which* failure this is decides
      // what the owner should do next, and getting that wrong sends them to
      // debug the wrong thing. Opening the shell on a machine that has never
      // paired is the single most common first-run state, and it used to read
      // "Is the daemon running?" while the daemon was running perfectly well
      // and answering 401.
      this.error.set(describeFailure(failure));
    } finally {
      this.busy.set(false);
    }
  }

  /**
   * Open a pairing window and show the code.
   *
   * The list is refreshed afterwards so a node that pairs appears without the
   * owner reloading the page — they are standing at the node, not at the
   * browser.
   */
  async pairNode(): Promise<void> {
    this.error.set(null);
    try {
      this.pairingWindow.set(await this.api.openPairingWindow());
    } catch {
      this.error.set('Could not open a pairing window.');
    }
  }

  /** Poll once for a newly paired device while a window is open. */
  async checkForNewDevice(): Promise<void> {
    const before = this.devices().length;
    await this.refresh();
    if (this.devices().length > before) {
      // The window is single-use and now spent; showing a dead code is worse
      // than showing none.
      this.pairingWindow.set(null);
    }
  }

  askToRevoke(deviceId: string): void {
    this.confirmingRevoke.set(deviceId);
  }

  cancelRevoke(): void {
    this.confirmingRevoke.set(null);
  }

  /** Revoke, once confirmed. Asked once — never twice, never zero times. */
  async confirmRevoke(deviceId: string): Promise<void> {
    this.confirmingRevoke.set(null);
    this.error.set(null);
    try {
      await this.api.revokeDevice(deviceId, 'revoked from settings');
      await this.refresh();
    } catch {
      this.error.set('Could not revoke that device.');
    }
  }

  /**
   * Choose the word the house answers to (ADR-032 §4).
   *
   * A `<select>` over what the server says is provisioned, never free text: a
   * word with no model is a node that has gone deaf, and the shell should not
   * be able to cause that by typing.
   */
  async setWakeWord(word: string): Promise<void> {
    this.error.set(null);
    try {
      this.voice.set(await this.api.updateVoiceSettings({ wakeWord: word }));
    } catch {
      this.error.set('Could not change the wake word.');
    }
  }

  /**
   * Granting consent is asked about once; withdrawing it is immediate.
   *
   * Deliberately asymmetric. Turning this on is the moment the house's voice
   * starts leaving the house (ADR-033 §2), and that deserves a beat. Turning
   * it off is the owner protecting themselves, and putting a confirmation in
   * front of that would be the interface arguing with them.
   */
  async toggleElevenLabs(): Promise<void> {
    const current = this.voice();
    if (!current) return;
    if (current.elevenlabs.enabled) {
      await this.applyElevenLabs(false);
      return;
    }
    this.confirmingElevenLabs.set(true);
  }

  cancelElevenLabs(): void {
    this.confirmingElevenLabs.set(false);
  }

  async confirmElevenLabs(): Promise<void> {
    this.confirmingElevenLabs.set(false);
    await this.applyElevenLabs(true);
  }

  private async applyElevenLabs(enabled: boolean): Promise<void> {
    this.error.set(null);
    try {
      this.voice.set(await this.api.updateVoiceSettings({ elevenlabsEnabled: enabled }));
    } catch {
      // The daemon refuses this when it is unconfigured or has no local voice
      // to fall back to (ADR-033 §3). Say which, without echoing a raw error.
      this.error.set(
        enabled
          ? 'Could not enable ElevenLabs. It needs an API key and a local voice to fall back to.'
          : 'Could not disable ElevenLabs.',
      );
    }
  }

  /** Spend as a percentage of the ceiling, for the meter. */
  readonly spendPercent = computed(() => {
    const eleven = this.voice()?.elevenlabs;
    if (!eleven || eleven.characterBudget === 0) return 0;
    return Math.min(100, Math.round((eleven.spentCharacters / eleven.characterBudget) * 100));
  });

  /**
   * Group digits without Angular's `DecimalPipe`.
   *
   * Same reason `when()` avoids `DatePipe`: the number pipe pulls the i18n
   * formatting machinery into the initial bundle even from this lazy route.
   * `toLocaleString` is in the browser and costs nothing.
   */
  thousands(value: number): string {
    return value.toLocaleString();
  }

  /** `2026-08` reads as `August 2026`. */
  periodLabel(period: string): string {
    const [year, month] = period.split('-').map(Number);
    if (!year || !month) return period;
    return new Date(Date.UTC(year, month - 1, 1)).toLocaleDateString(undefined, {
      month: 'long',
      year: 'numeric',
    });
  }

  async toggleAutomation(automation: AutomationDto): Promise<void> {
    this.error.set(null);
    try {
      await this.api.setAutomationEnabled(automation.id, !automation.enabled);
      await this.refresh();
    } catch {
      this.error.set('Could not change that automation.');
    }
  }

  /** Expand an automation's history, loading it the first time. */
  async toggleHistory(automation: AutomationDto): Promise<void> {
    if (this.expanded() === automation.id) {
      this.expanded.set(null);
      return;
    }
    this.expanded.set(automation.id);
    if (this.history()[automation.id] === undefined) {
      try {
        const response = await this.api.automationHistory(automation.id);
        this.history.update((all) => ({ ...all, [automation.id]: response.executions }));
      } catch {
        this.error.set('Could not load that automation’s history.');
      }
    }
  }

  historyFor(id: string): AutomationExecutionDto[] {
    return this.history()[id] ?? [];
  }

  /**
   * How a firing reads to a human.
   *
   * `denied` is deliberately not softened: "the automation was refused" and
   * "the automation ran and nothing happened" are indistinguishable from the
   * sofa, and this list is the only place that difference is visible.
   */
  outcomeLabel(outcome: string): string {
    switch (outcome) {
      case 'executed':
        return 'Ran';
      case 'denied':
        return 'Refused';
      case 'needs_approval':
        return 'Needed approval';
      case 'failed':
        return 'Failed';
      default:
        return outcome;
    }
  }

  triggerLabel(automation: AutomationDto): string {
    const trigger = automation.trigger as Record<string, unknown>;
    if (trigger['type'] === 'daily_at') {
      const minutes = Number(trigger['minutesSinceMidnight'] ?? 0);
      const hh = String(Math.floor(minutes / 60)).padStart(2, '0');
      const mm = String(minutes % 60).padStart(2, '0');
      return `Every day at ${hh}:${mm}`;
    }
    if (trigger['type'] === 'ha_state') {
      return `When ${String(trigger['entityId'])} becomes ${String(trigger['state'])}`;
    }
    return 'Unknown trigger';
  }

  isRevoked(device: DeviceDto): boolean {
    return Boolean(device.revokedAt);
  }

  /**
   * Format an RFC 3339 instant for display.
   *
   * `Intl.DateTimeFormat` rather than Angular's `DatePipe`, deliberately:
   * `DatePipe` drags the i18n formatting machinery into the **initial** bundle
   * even from a lazy route — measured at +12 kB, against a 500 kB budget this
   * shell is already over. `Intl` is in the browser and costs nothing.
   */
  when(instant: string | null | undefined): string {
    if (!instant) {
      return '';
    }
    const parsed = new Date(instant);
    if (Number.isNaN(parsed.getTime())) {
      return '';
    }
    return parsed.toLocaleString(undefined, {
      dateStyle: 'short',
      timeStyle: 'short',
    });
  }

  /** Just the time — for a pairing window that expires within the hour. */
  timeOnly(instant: string | null | undefined): string {
    if (!instant) {
      return '';
    }
    const parsed = new Date(instant);
    return Number.isNaN(parsed.getTime())
      ? ''
      : parsed.toLocaleTimeString(undefined, { timeStyle: 'short' });
  }
}

/**
 * Turn a failed request into something the owner can act on.
 *
 * The daemon already answers with an RFC 9457 problem body carrying a stable
 * code; the shell's job is to say what to *do*, not to echo it. Status 0 is
 * Angular's "the request never reached anybody".
 */
export function describeFailure(failure: unknown): string {
  const status = failure instanceof HttpErrorResponse ? failure.status : -1;
  switch (status) {
    case 0:
      return 'Could not reach the daemon. Is it running?';
    case 401:
      return 'This browser is not paired yet. Open the daemon’s health page on this machine to get the pairing code, then pair.';
    case 403:
      return 'This device is not allowed to administer settings. Use the owner’s browser.';
    default:
      return 'Could not load settings.';
  }
}

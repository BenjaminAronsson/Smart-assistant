import { Component, type OnInit, computed, inject, signal } from '@angular/core';
import type {
  AutomationDto,
  AutomationExecutionDto,
  DeviceDto,
  PairingWindowDto,
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

  async ngOnInit(): Promise<void> {
    await this.refresh();
  }

  async refresh(): Promise<void> {
    this.busy.set(true);
    this.error.set(null);
    try {
      const [devices, automations] = await Promise.all([
        this.api.listDevices(),
        this.api.listAutomations(),
      ]);
      this.devices.set(devices.devices);
      this.automations.set(automations.automations);
    } catch {
      // Never the raw error: an adapter/transport string has no business on a
      // settings page (docs/06 §5).
      this.error.set('Could not load settings. Is the daemon running?');
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

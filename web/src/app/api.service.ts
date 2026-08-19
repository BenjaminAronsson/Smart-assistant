import { HttpClient, HttpErrorResponse, HttpHeaders } from '@angular/common/http';
import { Injectable, inject } from '@angular/core';
import { firstValueFrom } from 'rxjs';
import type {
  ApprovalDecisionDto,
  AutomationHistoryResponse,
  AutomationListResponse,
  CreateSessionRequest,
  DeviceListResponse,
  HealthResponse,
  MapCoverageResponse,
  MediaCommandRequest,
  MediaCommandResponse,
  MediaStateResponse,
  PairRequest,
  PairResponse,
  PairingWindowDto,
  PolicyViewDto,
  ProvidersResponse,
  RunAck,
  RunDto,
  SessionDto,
  SessionListResponse,
  SubmitMessageRequest,
  TimelineResponse,
  UpdateVoiceSettingsRequest,
  VoiceSettingsDto,
} from '../generated/api-types';

const TOKEN_KEY = 'jarvis.deviceToken';

/**
 * Sentinel offered as the first WS subprotocol so `jarvisd::auth::
 * ws_subprotocol_token` can unambiguously tell "the next offered protocol is
 * a bearer token" from an ordinary subprotocol negotiation attempt. Must
 * match `jarvisd::auth::WS_DEVICE_TOKEN_PROTOCOL` exactly.
 */
const WS_DEVICE_TOKEN_PROTOCOL = 'jarvis.device.v1';

/**
 * Thin typed client over the jarvisd REST surface (docs/05 §1). All wire
 * shapes come from src/generated — never hand-written (ws-contracts skill).
 * The device token lives in localStorage for the M0 shell; keyring-backed
 * storage arrives with the desktop agent (docs/05 §6).
 */
@Injectable({ providedIn: 'root' })
export class ApiService {
  private readonly http = inject(HttpClient);

  health(): Promise<HealthResponse> {
    return firstValueFrom(this.http.get<HealthResponse>('/api/v1/diagnostics/health'));
  }

  hasToken(): boolean {
    return localStorage.getItem(TOKEN_KEY) !== null;
  }

  /**
   * Open an authenticated WebSocket at `path` (e.g. `/ws/v1`). A browser's
   * native `WebSocket` constructor has no way to set an `Authorization`
   * header on the handshake, so the device token travels as an offered
   * subprotocol instead, behind the {@link WS_DEVICE_TOKEN_PROTOCOL}
   * sentinel — `jarvisd::auth::ws_subprotocol_token` accepts that as a
   * fallback (only for a genuine WS handshake), and `ws::ws_upgrade` echoes
   * just the sentinel back to complete it, never the token itself.
   */
  openSocket(path: string): WebSocket {
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${protocol}//${window.location.host}${path}`;
    const token = localStorage.getItem(TOKEN_KEY);
    return token !== null ? new WebSocket(url, [WS_DEVICE_TOKEN_PROTOCOL, token]) : new WebSocket(url);
  }

  async pair(pairingCode: string, deviceName: string): Promise<PairResponse> {
    const request: PairRequest = { pairingCode, deviceName };
    const response = await firstValueFrom(
      this.http.post<PairResponse>('/api/v1/auth/pair', request),
    );
    localStorage.setItem(TOKEN_KEY, response.deviceToken);
    return response;
  }

  createSession(title: string | undefined, idempotencyKey?: string): Promise<SessionDto> {
    const request: CreateSessionRequest = title === undefined ? {} : { title };
    let headers = this.authHeaders();
    if (idempotencyKey !== undefined) {
      headers = headers.set('Idempotency-Key', idempotencyKey);
    }
    return firstValueFrom(
      this.http.post<SessionDto>('/api/v1/sessions', request, { headers }),
    );
  }

  getSession(id: string): Promise<SessionDto> {
    return firstValueFrom(
      this.http.get<SessionDto>(`/api/v1/sessions/${id}`, { headers: this.authHeaders() }),
    );
  }

  listSessions(): Promise<SessionListResponse> {
    return firstValueFrom(
      this.http.get<SessionListResponse>('/api/v1/sessions', { headers: this.authHeaders() }),
    );
  }

  getTimeline(sessionId: string, since = 0): Promise<TimelineResponse> {
    const params: Record<string, string | number> = since > 0 ? { since } : {};
    return firstValueFrom(
      this.http.get<TimelineResponse>(`/api/v1/sessions/${sessionId}/timeline`, {
        params,
        headers: this.authHeaders(),
      }),
    );
  }

  /** Durable run snapshot used to repair a HUD stream gap (docs/05 §1/§3). */
  getRun(runId: string): Promise<RunDto> {
    return firstValueFrom(
      this.http.get<RunDto>(`/api/v1/runs/${runId}`, { headers: this.authHeaders() }),
    );
  }

  submitMessage(sessionId: string, text: string): Promise<RunAck> {
    const request: SubmitMessageRequest = {
      content: [{ type: 'text', text }],
    };
    return firstValueFrom(
      this.http.post<RunAck>(`/api/v1/sessions/${sessionId}/messages`, request, {
        headers: this.authHeaders(),
      }),
    );
  }

  /**
   * Resolve a pending approval (docs/05 §4). The body echoes the approval id in
   * the path; on `approve`, `editedArguments` rebinds the grant to the edited set
   * (docs/06 §4). A 200 only means the decision was recorded — the run's actual
   * unblocking is observed via the `approval.resolved` WS event, so callers keep
   * the card optimistically blocked until that durable event arrives.
   */
  async resolveApproval(
    runId: string,
    approvalId: string,
    decision: ApprovalDecisionDto,
  ): Promise<void> {
    await firstValueFrom(
      this.http.post(`/api/v1/runs/${runId}/approvals/${approvalId}`, decision, {
        headers: this.authHeaders(),
      }),
    );
  }

  /** Request cancellation of the active run; completion is confirmed by WS. */
  async cancelRun(runId: string): Promise<void> {
    await firstValueFrom(
      this.http.post(`/api/v1/runs/${runId}/cancel`, {}, { headers: this.authHeaders() }),
    );
  }

  /**
   * Current local playback state (FR-22). Needed on connect because
   * `media.state` is a *transient* WS event and is never replayed (docs/05 §3):
   * the bar reads this once, then follows events.
   */
  getMediaState(): Promise<MediaStateResponse> {
    return firstValueFrom(
      this.http.get<MediaStateResponse>('/api/v1/media/state', { headers: this.authHeaders() }),
    );
  }

  /**
   * Apply a transport command (exit evidence #4). Omitting `player` targets the
   * unambiguous active player; the server refuses (409) rather than guessing
   * when two players are active, and refuses any volume above the configured
   * cap — the bar never overrides either decision locally.
   */
  sendMediaCommand(request: MediaCommandRequest): Promise<MediaCommandResponse> {
    return firstValueFrom(
      this.http.post<MediaCommandResponse>('/api/v1/media/command', request, {
        headers: this.authHeaders(),
      }),
    );
  }

  getProviders(): Promise<ProvidersResponse> {
    return firstValueFrom(
      this.http.get<ProvidersResponse>('/api/v1/providers', { headers: this.authHeaders() }),
    );
  }

  /**
   * The locally served PMTiles archive's coverage (F3b.5, docs/12 §3,
   * ADR-013). `null` means "no local map configured" (jarvisd registers no
   * map routes at all when `[maps] pmtiles_path` is unset, so this is a 404 —
   * absent, not broken; docs/09 §1). Any other failure (network error, 5xx)
   * rethrows, so the map card's fallback logic can tell "no archive" apart
   * from "could not ask" and degrade to the safer of the two (coordinates-only)
   * rather than assuming online raster is reachable.
   */
  async getMapCoverage(): Promise<MapCoverageResponse | null> {
    try {
      return await firstValueFrom(
        this.http.get<MapCoverageResponse>('/api/v1/map/coverage', {
          headers: this.authHeaders(),
        }),
      );
    } catch (e) {
      if (e instanceof HttpErrorResponse && e.status === 404) {
        return null;
      }
      throw e;
    }
  }

  // --- settings surface (F8.8) ------------------------------------------

  /** Every device, revoked ones included — a revoked device is history, not a gap. */
  listDevices(): Promise<DeviceListResponse> {
    return firstValueFrom(
      this.http.get<DeviceListResponse>('/api/v1/devices', { headers: this.authHeaders() }),
    );
  }

  /**
   * Open a pairing window and get the one-time code to read out (ADR-031 §5).
   * The owner is already authenticated at a keyboard when they pair a
   * satellite; that is the ceremony.
   */
  openPairingWindow(): Promise<PairingWindowDto> {
    return firstValueFrom(
      this.http.post<PairingWindowDto>(
        '/api/v1/devices/pairing-window',
        {},
        { headers: this.authHeaders() },
      ),
    );
  }

  revokeDevice(id: string, reason: string): Promise<void> {
    return firstValueFrom(
      this.http.post<void>(
        `/api/v1/devices/${id}/revoke`,
        { reason },
        { headers: this.authHeaders() },
      ),
    );
  }

  listAutomations(): Promise<AutomationListResponse> {
    return firstValueFrom(
      this.http.get<AutomationListResponse>('/api/v1/automations', {
        headers: this.authHeaders(),
      }),
    );
  }

  setAutomationEnabled(id: string, enabled: boolean): Promise<void> {
    return firstValueFrom(
      this.http.patch<void>(
        `/api/v1/automations/${id}`,
        { enabled },
        { headers: this.authHeaders() },
      ),
    );
  }

  /**
   * The read-only policy view (F10.5, FR-05): what each tool may do, and what
   * each device class is actually allowed.
   *
   * Read-only by design — there is no counterpart write method, and adding one
   * needs an ADR first (see `jarvis_contracts::policy`). Changing a risk tier
   * from a web page is a far larger authority question than changing a wake
   * word.
   */
  getPolicy(): Promise<PolicyViewDto> {
    return firstValueFrom(
      this.http.get<PolicyViewDto>('/api/v1/policy', {
        headers: this.authHeaders(),
      }),
    );
  }

  /** The owner-tunable voice settings (F8.8, F8.11). */
  getVoiceSettings(): Promise<VoiceSettingsDto> {
    return firstValueFrom(
      this.http.get<VoiceSettingsDto>('/api/v1/settings/voice', {
        headers: this.authHeaders(),
      }),
    );
  }

  /**
   * Change them. Absent fields mean unchanged, so one toggle does not restate
   * — and overwrite — whatever another tab just set.
   */
  updateVoiceSettings(patch: UpdateVoiceSettingsRequest): Promise<VoiceSettingsDto> {
    return firstValueFrom(
      this.http.patch<VoiceSettingsDto>('/api/v1/settings/voice', patch, {
        headers: this.authHeaders(),
      }),
    );
  }

  automationHistory(id: string): Promise<AutomationHistoryResponse> {
    return firstValueFrom(
      this.http.get<AutomationHistoryResponse>(`/api/v1/automations/${id}/history`, {
        headers: this.authHeaders(),
      }),
    );
  }

  private authHeaders(): HttpHeaders {
    const token = localStorage.getItem(TOKEN_KEY);
    return token
      ? new HttpHeaders({ Authorization: `Bearer ${token}` })
      : new HttpHeaders();
  }
}

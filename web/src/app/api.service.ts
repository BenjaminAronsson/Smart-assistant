import { HttpClient, HttpErrorResponse, HttpHeaders } from '@angular/common/http';
import { Injectable, inject } from '@angular/core';
import { firstValueFrom } from 'rxjs';
import type {
  CreateSessionRequest,
  HealthResponse,
  PairRequest,
  PairResponse,
  SessionDto,
  SessionListResponse,
  TimelineResponse,
  SubmitMessageRequest,
  ProvidersResponse,
  RunAck,
  ApprovalDecisionDto,
  MediaCommandRequest,
  MediaCommandResponse,
  MediaStateResponse,
  MapCoverageResponse,
} from '../generated/api-types';

const TOKEN_KEY = 'jarvis.deviceToken';

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

  private authHeaders(): HttpHeaders {
    const token = localStorage.getItem(TOKEN_KEY);
    return token
      ? new HttpHeaders({ Authorization: `Bearer ${token}` })
      : new HttpHeaders();
  }
}

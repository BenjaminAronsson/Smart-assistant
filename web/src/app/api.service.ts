import { HttpClient, HttpHeaders } from '@angular/common/http';
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

  getProviders(): Promise<ProvidersResponse> {
    return firstValueFrom(
      this.http.get<ProvidersResponse>('/api/v1/providers', { headers: this.authHeaders() }),
    );
  }

  private authHeaders(): HttpHeaders {
    const token = localStorage.getItem(TOKEN_KEY);
    return token
      ? new HttpHeaders({ Authorization: `Bearer ${token}` })
      : new HttpHeaders();
  }
}

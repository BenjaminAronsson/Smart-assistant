import { HttpClient, HttpHeaders } from '@angular/common/http';
import { Injectable, inject } from '@angular/core';
import { firstValueFrom } from 'rxjs';
import type {
  CapabilityResultDto,
  CapabilityTokenDto,
} from '../../generated/api-types';

const TOKEN_KEY = 'jarvis.deviceToken';

/** The one message type a generated app may send (F6.5, docs/06 §6). */
export const CAPABILITY_REQUEST = 'jarvis.capability.request';
/** The one message type the shell sends back. */
export const CAPABILITY_RESULT = 'jarvis.capability.result';

/**
 * A request as it arrives from the frame. **Untrusted**: every field is
 * model-influenced content, so nothing here is used for anything but being
 * validated and forwarded to the host, which decides.
 */
export interface CapabilityRequestMessage {
  readonly type: typeof CAPABILITY_REQUEST;
  /** Correlates the reply. Echoed back verbatim; never interpreted. */
  readonly id: string;
  readonly capability: string;
  readonly target: string;
  readonly value?: string;
}

export interface CapabilityResultMessage {
  readonly type: typeof CAPABILITY_RESULT;
  readonly id: string;
  readonly ok: boolean;
  readonly content?: string;
  /** A stable machine code (`app.undeclared_capability`, …) — never a
   * server-authored sentence, which an app could render as if it were the
   * shell's own words. */
  readonly code?: string;
}

/**
 * The shell's half of the capability bridge (F6.5, ADR-030).
 *
 * A generated app runs in an opaque origin under `connect-src 'none'`: it
 * cannot reach jarvisd, and it holds no credential to reach it with. So it
 * `postMessage`s here, and this service — which does hold the device token —
 * makes the call. **It grants nothing.** It mints a short-lived, single-use
 * capability token for the named capability and exchanges it; jarvisd re-checks
 * the app's manifest, runs `policy::evaluate`, asks for approval where the tier
 * demands it, and mints a real `ExecutionGrant` for R2+.
 *
 * Two message-hygiene rules the caller must uphold, both consequences of the
 * opaque origin (see ADR-030's last consequence):
 * * inbound, `event.origin` is the string `"null"` and therefore useless —
 *   identity is established by comparing `event.source` to the frame's
 *   `contentWindow`;
 * * outbound, `targetOrigin` must be `'*'`, because an opaque origin cannot be
 *   named. That is safe **only** because the frame is single-purpose and
 *   sandboxed, and only ever carries a reply to a request that frame just made.
 */
@Injectable({ providedIn: 'root' })
export class AppBridgeService {
  private readonly http = inject(HttpClient);

  /**
   * Is this a well-formed capability request? Shape-checks only — the host
   * owns every semantic decision, and duplicating one here would create a
   * second, weaker rule to keep in sync.
   */
  isCapabilityRequest(data: unknown): data is CapabilityRequestMessage {
    if (typeof data !== 'object' || data === null) return false;
    const m = data as Record<string, unknown>;
    return (
      m['type'] === CAPABILITY_REQUEST &&
      typeof m['id'] === 'string' &&
      typeof m['capability'] === 'string' &&
      typeof m['target'] === 'string' &&
      (m['value'] === undefined || typeof m['value'] === 'string')
    );
  }

  /**
   * Mint a token and spend it on one operation. Returns the reply to post back
   * into the frame — never throws at the caller, because a rejection is a
   * normal outcome the app must be able to render.
   */
  async fulfil(
    artifactId: string,
    version: number,
    request: CapabilityRequestMessage,
  ): Promise<CapabilityResultMessage> {
    try {
      const minted = await firstValueFrom(
        this.http.post<CapabilityTokenDto>(
          `/api/v1/apps/${artifactId}/versions/${version}/capability-tokens`,
          { capability: request.capability },
          { headers: this.authHeaders() },
        ),
      );
      const result = await firstValueFrom(
        this.http.post<CapabilityResultDto>(
          `/api/v1/apps/${artifactId}/versions/${version}/invoke`,
          {
            capability: request.capability,
            target: request.target,
            ...(request.value === undefined ? {} : { value: request.value }),
            token: minted.token,
          },
          { headers: this.authHeaders() },
        ),
      );
      return {
        type: CAPABILITY_RESULT,
        id: request.id,
        ok: true,
        content: result.content,
      };
    } catch (err: unknown) {
      return {
        type: CAPABILITY_RESULT,
        id: request.id,
        ok: false,
        code: problemCode(err),
      };
    }
  }

  private authHeaders(): HttpHeaders {
    const token = localStorage.getItem(TOKEN_KEY);
    return token ? new HttpHeaders({ Authorization: `Bearer ${token}` }) : new HttpHeaders();
  }
}

/** The stable machine code from an RFC 9457 body, or a generic one. Only the
 * code crosses back into the app: a server sentence rendered inside a generated
 * app would read as the shell speaking. */
function problemCode(err: unknown): string {
  const body = (err as { error?: unknown } | null)?.error;
  if (typeof body === 'object' && body !== null) {
    const code = (body as Record<string, unknown>)['code'];
    if (typeof code === 'string') return code;
  }
  return 'app.request_failed';
}

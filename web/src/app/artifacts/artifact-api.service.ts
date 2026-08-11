import { HttpClient, HttpHeaders } from '@angular/common/http';
import { Injectable, inject } from '@angular/core';
import { firstValueFrom } from 'rxjs';
import type { ArtifactVersionsResponse } from '../../generated/api-types';

/** Same device-token key `ApiService` uses (docs/05 §1). Not imported from
 * there — this feature's scope is `web/src/app/artifacts/` plus
 * `app.routes.ts` (F3b.3 scope boundary), so this is its own thin client
 * rather than a shared-file edit that could collide with the parallel HUD
 * card-grammar and timers work. */
const TOKEN_KEY = 'jarvis.deviceToken';

/**
 * Thin typed client over the artifact **read** surface (`crates/jarvisd/src/
 * artifacts.rs`): list a manifest's versions and fetch one version's blob.
 * Creation/promotion endpoints are out of scope here — artifacts are run
 * outputs, never POSTed by this client (docs/05 §1).
 */
@Injectable({ providedIn: 'root' })
export class ArtifactApiService {
  private readonly http = inject(HttpClient);

  getVersions(id: string): Promise<ArtifactVersionsResponse> {
    return firstValueFrom(
      this.http.get<ArtifactVersionsResponse>(`/api/v1/artifacts/${id}/versions`, {
        headers: this.authHeaders(),
      }),
    );
  }

  /** Fetch a version's blob as text — Markdown/HTML, code/text, and chart
   * (JSON) artifacts are all textual. */
  getBlobText(id: string, version: number): Promise<string> {
    return firstValueFrom(
      this.http.get(`/api/v1/artifacts/${id}/versions/${version}/blob`, {
        headers: this.authHeaders(),
        responseType: 'text',
      }),
    );
  }

  /** Fetch a version's blob as a `Blob` — used for image artifacts, which the
   * renderer turns into an object URL (never a direct navigation to this
   * URL, which the server deliberately serves as `Content-Disposition:
   * attachment` to prevent, docs/06 §6). */
  getBlobBlob(id: string, version: number): Promise<Blob> {
    return firstValueFrom(
      this.http.get(`/api/v1/artifacts/${id}/versions/${version}/blob`, {
        headers: this.authHeaders(),
        responseType: 'blob',
      }),
    );
  }

  /** Fetch a `bundle` version as an **app document** (F6.4, ADR-030): the
   * separate, deliberately renderable route, which serves the bundle under a
   * restrictive CSP with the host's policy `<meta>` prepended. Deliberately not
   * `…/blob`, which stays `Content-Disposition: attachment` for every kind —
   * the two routes exist so the download path never has to be relaxed. */
  getAppDocument(id: string, version: number): Promise<string> {
    return firstValueFrom(
      this.http.get(`/api/v1/apps/${id}/versions/${version}/document`, {
        headers: this.authHeaders(),
        responseType: 'text',
      }),
    );
  }

  private authHeaders(): HttpHeaders {
    const token = localStorage.getItem(TOKEN_KEY);
    return token ? new HttpHeaders({ Authorization: `Bearer ${token}` }) : new HttpHeaders();
  }
}

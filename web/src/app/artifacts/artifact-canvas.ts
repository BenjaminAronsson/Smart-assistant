import { HttpErrorResponse } from '@angular/common/http';
import { ChangeDetectionStrategy, Component, OnInit, computed, inject, signal } from '@angular/core';
import { ActivatedRoute } from '@angular/router';
import type { ArtifactManifestDto, ArtifactSourceDto } from '../../generated/api-types';
import { ArtifactApiService } from './artifact-api.service';
import { CodeRenderer } from './renderers/code-renderer';
import { ChartRenderer } from './renderers/chart-renderer';
import { ImageRenderer } from './renderers/image-renderer';
import { MarkdownRenderer } from './renderers/markdown-renderer';
import { UnsupportedRenderer } from './renderers/unsupported-renderer';
import { sanitizeHref } from './safe-url';

function isNotFound(err: unknown): boolean {
  return err instanceof HttpErrorResponse && err.status === 404;
}

/**
 * The ArtifactCanvas surface (docs/02 §6, F3b.3): loads an artifact's version
 * list, renders the latest version by its `ArtifactKind`, and lets the user
 * step to an older version. This is also the exit-evidence-#1 UI: reopening
 * an artifact created before a restart shows its content **and** its
 * provenance (sources, sensitivity, build) — the manifest is durable, the
 * blob is content-addressed, so there is nothing special about "after
 * restart" from this component's point of view, which is the point.
 *
 * Own route (`/artifacts/:id`, lazy — `app.routes.ts`); reachable directly by
 * URL/`router.navigate`. Wiring a HUD-card "open artifact" affordance into
 * this route is out of this feature's scope (F3b.2/F3b.6 own the card
 * grammar and the deep-dive handoff) — noted as a deviation in the PR.
 */
@Component({
  selector: 'app-artifact-canvas',
  imports: [MarkdownRenderer, CodeRenderer, ImageRenderer, ChartRenderer, UnsupportedRenderer],
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './artifact-canvas.html',
  styleUrl: './artifact-canvas.scss',
})
export class ArtifactCanvas implements OnInit {
  private readonly route = inject(ActivatedRoute);
  private readonly api = inject(ArtifactApiService);

  protected readonly artifactId = signal<string | null>(null);
  protected readonly versions = signal<ArtifactManifestDto[]>([]);
  protected readonly selectedVersion = signal<number | null>(null);
  /** Set for every kind except `image` (Markdown/HTML, code/text and chart
   * are all textual; `bundle` and unknown kinds never fetch bytes at all). */
  protected readonly textContent = signal<string | null>(null);
  protected readonly imageBlob = signal<Blob | null>(null);
  protected readonly loading = signal(false);
  protected readonly error = signal<string | null>(null);

  protected readonly selectedManifest = computed<ArtifactManifestDto | null>(
    () => this.versions().find((v) => v.version === this.selectedVersion()) ?? null,
  );

  ngOnInit(): void {
    const id = this.route.snapshot.paramMap.get('id');
    if (!id) {
      this.error.set('No artifact id in the route.');
      return;
    }
    this.artifactId.set(id);
    void this.loadVersions(id);
  }

  private async loadVersions(id: string): Promise<void> {
    this.loading.set(true);
    this.error.set(null);
    try {
      const resp = await this.api.getVersions(id);
      this.versions.set(resp.versions);
      // "oldest first" (docs/05 §1) ⇒ the latest is the last entry.
      const latest = resp.versions.at(-1) ?? null;
      if (latest === null) {
        this.error.set('This artifact has no versions.');
        return;
      }
      await this.selectVersion(latest.version);
    } catch (err) {
      this.error.set(isNotFound(err) ? 'Artifact not found.' : 'Could not load this artifact.');
    } finally {
      this.loading.set(false);
    }
  }

  protected async selectVersion(version: number): Promise<void> {
    const id = this.artifactId();
    const manifest = this.versions().find((v) => v.version === version);
    if (id === null || manifest === undefined) return;

    this.selectedVersion.set(version);
    this.textContent.set(null);
    this.imageBlob.set(null);
    this.error.set(null);

    // Bundle (and any future unrecognized kind) never fetches bytes — the
    // unsupported-renderer says so without a network round-trip.
    if (manifest.kind === 'bundle') return;

    this.loading.set(true);
    try {
      if (manifest.kind === 'image') {
        this.imageBlob.set(await this.api.getBlobBlob(id, version));
      } else {
        this.textContent.set(await this.api.getBlobText(id, version));
      }
    } catch {
      this.error.set('Could not load this version.');
    } finally {
      this.loading.set(false);
    }
  }

  /** A `web` source's reference is a URL from provenance that ultimately
   * traces back to a fetched page — untrusted enough to sanitize the same
   * way an artifact-body link is (F3b.3 threat note extends to provenance,
   * not just blob bytes). `message`/`run` references are internal ULIDs, not
   * navigation targets, so they render as plain text. */
  protected sourceHref(source: ArtifactSourceDto): string | null {
    return source.kind === 'web' ? sanitizeHref(source.reference) : null;
  }

  protected trackByVersion(_index: number, v: ArtifactManifestDto): number {
    return v.version;
  }
}

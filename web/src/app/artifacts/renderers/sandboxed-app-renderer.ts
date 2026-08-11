import { ChangeDetectionStrategy, Component, computed, input } from '@angular/core';
import { DomSanitizer, type SafeHtml } from '@angular/platform-browser';
import { inject } from '@angular/core';

/**
 * Generated-app renderer (`ArtifactKindDto.bundle`, renderer
 * `sandboxed-webapp/v1`) — F6.4, FR-18, docs/06 §6, **ADR-030**.
 *
 * A bundle is model-influenced code. It runs in a frame with
 * `sandbox="allow-scripts"` and **no `allow-same-origin`**, which gives the
 * document a unique **opaque origin**: it cannot read this page's DOM, its
 * `localStorage` (where the device token lives), or call `/api/v1` as the user.
 * The document jarvisd served also carries a restrictive CSP of its own
 * (`default-src 'none'; connect-src 'none'; …`), so the app cannot reach the
 * network either.
 *
 * **Do not add `allow-same-origin` to that attribute.** With `allow-scripts` it
 * lets the frame remove its own sandbox, and the whole boundary is gone — this
 * is the single most likely way this feature gets destroyed later, so the token
 * set is asserted verbatim by a test that says so. The attribute is *static* in
 * the template: Angular refuses to bind `sandbox` at all (NG0910), so there is
 * no runtime value that could widen it.
 *
 * `bypassSecurityTrustHtml` looks alarming and is correct here: Angular's
 * sanitizer would strip the app's own inline script, and the app *is* its
 * script. Angular's sanitizer is not the boundary — the opaque origin and the
 * CSP are. What matters is that these bytes reach **only** `[srcdoc]` on this
 * sandboxed frame and never `innerHTML` anywhere in the control origin.
 */
@Component({
  selector: 'app-sandboxed-app-renderer',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './sandboxed-app-renderer.html',
  styleUrl: './sandboxed-app-renderer.scss',
})
export class SandboxedAppRenderer {
  /** The app document, exactly as jarvisd's `/api/v1/apps/…/document` served
   * it — CSP `<meta>` first, then the bundle. */
  readonly document = input.required<string>();
  /** Accessible name for the frame. */
  readonly label = input<string>('Generated app');

  private readonly sanitizer = inject(DomSanitizer);

  protected readonly srcdoc = computed<SafeHtml>(() =>
    this.sanitizer.bypassSecurityTrustHtml(this.document()),
  );
}

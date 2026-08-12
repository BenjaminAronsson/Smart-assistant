import {
  ChangeDetectionStrategy,
  Component,
  DestroyRef,
  ElementRef,
  computed,
  inject,
  input,
  viewChild,
} from '@angular/core';
import { DomSanitizer, type SafeHtml } from '@angular/platform-browser';
import { AppBridgeService, CAPABILITY_RESULT } from '../app-bridge.service';

/**
 * Generated-app renderer (`ArtifactKindDto.bundle`, renderer
 * `sandboxed-webapp/v1`) — F6.4/F6.5, FR-18, docs/06 §6, **ADR-030**.
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
 *
 * ## The bridge (F6.5)
 *
 * The frame may `postMessage` one thing: a request naming a capability its own
 * manifest declares. This component forwards it to
 * [`AppBridgeService`] — which mints a single-use token and lets **jarvisd**
 * decide — and posts the reply back. It authorizes nothing itself.
 *
 * Two rules follow from the opaque origin and are load-bearing:
 * * inbound, `event.origin` is the literal string `"null"` and proves nothing;
 *   the message is accepted only if `event.source` **is** this frame's
 *   `contentWindow`. Any other window is ignored silently — a page cannot learn
 *   whether an app is open by probing.
 * * outbound, `targetOrigin` must be `'*'` because an opaque origin cannot be
 *   named. Safe only because the target is this one sandboxed frame and the
 *   payload is a reply to a request it just made.
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
  /** Which app this is, so the bridge can scope its tokens. */
  readonly artifactId = input.required<string>();
  readonly version = input.required<number>();

  private readonly sanitizer = inject(DomSanitizer);
  private readonly bridge = inject(AppBridgeService);
  private readonly frame = viewChild.required<ElementRef<HTMLIFrameElement>>('frame');

  protected readonly srcdoc = computed<SafeHtml>(() =>
    this.sanitizer.bypassSecurityTrustHtml(this.document()),
  );

  constructor() {
    const listener = (event: MessageEvent): void => void this.onMessage(event);
    window.addEventListener('message', listener);
    inject(DestroyRef).onDestroy(() => window.removeEventListener('message', listener));
  }

  private async onMessage(event: MessageEvent): Promise<void> {
    // Identity, not origin: an opaque-origin frame posts with `origin: "null"`,
    // which every sandboxed frame on the page shares. `contentWindow` identity
    // is the only thing that distinguishes *this* app from any other.
    const frame = this.frame().nativeElement;
    if (event.source === null || event.source !== frame.contentWindow) return;
    if (!this.bridge.isCapabilityRequest(event.data)) return;

    const reply = await this.bridge.fulfil(this.artifactId(), this.version(), event.data);
    // `'*'` is required — an opaque origin has no name to target. Bounded by
    // posting only to this frame, and only a `CAPABILITY_RESULT` envelope.
    frame.contentWindow?.postMessage(reply, '*');
  }

  /** Exposed for the spec: the reply envelope type is part of the contract. */
  protected readonly resultType = CAPABILITY_RESULT;
}

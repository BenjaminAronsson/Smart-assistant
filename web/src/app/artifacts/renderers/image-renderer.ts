import { ChangeDetectionStrategy, Component, OnDestroy, effect, input, signal } from '@angular/core';

/**
 * Image artifact renderer (`ArtifactKindDto.image`). The blob is rendered
 * through a same-origin `blob:` object URL bound to a plain `<img>` — never
 * `<object>`, `<embed>`, or an `<iframe>`, and never `[innerHTML]`/
 * `bypassSecurityTrust*`. This matters even for `image/svg+xml`: an SVG
 * loaded as the *source of an `<img>` element* does not execute embedded
 * `<script>` (the HTML spec disables scripting for image contexts) — the
 * risk only exists if SVG bytes are ever inlined as markup or loaded as a
 * top-level document/frame, which this component never does (F3b.3 threat
 * note).
 */
@Component({
  selector: 'app-image-renderer',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './image-renderer.html',
  styleUrl: './image-renderer.scss',
})
export class ImageRenderer implements OnDestroy {
  readonly blob = input.required<Blob>();
  readonly label = input('Artifact image');

  protected readonly objectUrl = signal<string | null>(null);
  private currentUrl: string | null = null;

  constructor() {
    effect(() => {
      const blob = this.blob();
      this.revoke();
      const next = URL.createObjectURL(blob);
      this.currentUrl = next;
      this.objectUrl.set(next);
    });
  }

  ngOnDestroy(): void {
    this.revoke();
  }

  private revoke(): void {
    if (this.currentUrl !== null) {
      URL.revokeObjectURL(this.currentUrl);
      this.currentUrl = null;
    }
  }
}

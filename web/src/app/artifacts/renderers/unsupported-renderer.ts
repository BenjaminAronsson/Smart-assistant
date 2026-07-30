import { ChangeDetectionStrategy, Component, computed, input } from '@angular/core';
import type { ArtifactKindDto } from '../../../generated/api-types';

const LABEL: Readonly<Record<ArtifactKindDto, string>> = Object.freeze({
  markdown_html: 'Markdown/HTML',
  code_text: 'Code/text',
  image: 'Image',
  chart: 'Chart',
  bundle: 'Generated app (bundle)',
});

/**
 * Explicit "not supported here" state (docs/02 §6, F3b.3 scope boundary).
 * `bundle` artifacts (generated web apps) are M6 sandbox work — this
 * component never attempts to fetch or display their bytes, only says so. It
 * doubles as the fallback for any kind this build doesn't otherwise handle,
 * so an unrecognized/future kind degrades to a message, never a blank panel.
 */
@Component({
  selector: 'app-unsupported-renderer',
  changeDetection: ChangeDetectionStrategy.OnPush,
  template: `
    <div class="unsupported" role="status">
      <p>
        This artifact is a <strong>{{ label() }}</strong
        >. It isn't rendered here yet.
      </p>
      @if (kind() === 'bundle') {
        <p class="hint">Generated apps run only in their own sandbox (M6) — not on this canvas.</p>
      }
    </div>
  `,
  styleUrl: './unsupported-renderer.scss',
})
export class UnsupportedRenderer {
  readonly kind = input.required<ArtifactKindDto>();

  protected readonly label = computed(() => LABEL[this.kind()] ?? 'unknown kind');
}

import { ChangeDetectionStrategy, Component, input } from '@angular/core';

/**
 * Code/text artifact renderer (`ArtifactKindDto.code_text`). The blob is
 * arbitrary text — a coding-worker patch, a log, a config file — and is never
 * anything but text: bound through `{{ }}` interpolation into a `<pre><code>`
 * so it is always literal content, never markup (F3b.3 threat note).
 */
@Component({
  selector: 'app-code-renderer',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './code-renderer.html',
  styleUrl: './code-renderer.scss',
})
export class CodeRenderer {
  readonly content = input.required<string>();
  /** The manifest's media type, shown as a label (e.g. "text/x-diff"). */
  readonly mediaType = input<string | null>(null);
}

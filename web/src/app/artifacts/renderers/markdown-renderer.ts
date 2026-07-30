import { ChangeDetectionStrategy, Component, computed, input } from '@angular/core';
import { type MdBlock, parseMarkdown } from '../markdown/markdown-parser';
import { MarkdownInline } from './markdown-inline';

/**
 * Markdown/HTML artifact renderer (docs/02 §6, `ArtifactKindDto.markdown_html`).
 *
 * Despite the DTO's name, this never renders HTML — the "HTML" half of the
 * kind describes the artifact's declared media type, not a licence to inject
 * markup. Artifact bytes are untrusted (F3b.3 threat note: they can come from
 * a fetched web page, a coding-worker output, or a deep-dive promotion), so
 * the whole render path goes through {@link parseMarkdown}'s safe-subset
 * parser and Angular's auto-escaping text bindings. There is no
 * `[innerHTML]`, no `bypassSecurityTrust*`, anywhere in this component.
 */
@Component({
  selector: 'app-markdown-renderer',
  imports: [MarkdownInline],
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './markdown-renderer.html',
  styleUrl: './markdown-renderer.scss',
})
export class MarkdownRenderer {
  readonly content = input.required<string>();

  protected readonly blocks = computed<MdBlock[]>(() => parseMarkdown(this.content()));

  protected trackByIndex(index: number): number {
    return index;
  }
}

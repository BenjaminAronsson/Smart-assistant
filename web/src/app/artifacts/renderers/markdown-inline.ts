import { ChangeDetectionStrategy, Component, input } from '@angular/core';
import type { MdInline } from '../markdown/markdown-parser';

/**
 * Renders one run of parsed inline markdown nodes. Every branch below binds
 * `node.value` through a text interpolation — never `[innerHTML]`, never
 * `bypassSecurityTrust*` — so artifact bytes can only ever land in the DOM as
 * literal characters, whatever they contain (docs/02 §6, F3b.3 threat note).
 * `link` nodes carry an href that has already passed `sanitizeHref`; this
 * component trusts that invariant and does not re-validate it.
 */
@Component({
  selector: 'app-md-inline',
  changeDetection: ChangeDetectionStrategy.OnPush,
  template: `
    @for (node of nodes(); track $index) {
      @switch (node.type) {
        @case ('text') {
          {{ node.value }}
        }
        @case ('strong') {
          <strong>{{ node.value }}</strong>
        }
        @case ('em') {
          <em>{{ node.value }}</em>
        }
        @case ('code') {
          <code>{{ node.value }}</code>
        }
        @case ('link') {
          <a [href]="asLink(node).href" target="_blank" rel="noopener noreferrer nofollow">{{
            node.value
          }}</a>
        }
      }
    }
  `,
})
export class MarkdownInline {
  readonly nodes = input.required<MdInline[]>();

  /** Narrows the union for the template's `link` branch (strictTemplates
   * cannot narrow through `@switch` on its own for a discriminated union
   * accessed via a getter). */
  protected asLink(node: MdInline): { href: string } {
    return node as { type: 'link'; href: string; value: string };
  }
}

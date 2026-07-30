import { ChangeDetectionStrategy, Component, input } from '@angular/core';
import type { SourcedImageDto } from '../../../generated/api-types';
import { SourceChip } from './source-chip';

/**
 * The only way a card shows a web-sourced photo (docs/12 §2.3, FR-25/ADR-014).
 * `SourcedImageDto` requires `sourceUrl`/`sourceDomain`/`alt` at the wire-type
 * level, and this is the **only** renderer for that type — so there is no
 * path in this codebase that paints a `SourcedImageDto`'s `url` without also
 * rendering its chip and alt text alongside it.
 */
@Component({
  selector: 'app-sourced-image',
  imports: [SourceChip],
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './sourced-image.html',
  styleUrl: './sourced-image.scss',
})
export class SourcedImage {
  readonly image = input.required<SourcedImageDto>();
}

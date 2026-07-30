import { ChangeDetectionStrategy, Component, input } from '@angular/core';

/**
 * The visible source-link chip (docs/12 §2.3, FR-25/ADR-014): "wikipedia.org
 * ↗". A real, focusable anchor — not a styled span — so the attribution is
 * keyboard-reachable and has its own visible focus ring (docs/12 §8). `domain`
 * and `href` are interpolated as plain text/attribute values; Angular's
 * built-in URL sanitization on `[href]` is the only thing standing between an
 * untrusted `sourceUrl` and the anchor, and that is enough — there is no
 * `innerHTML`/`bypassSecurityTrust*` on this surface (docs/12 §9, invariant 1).
 */
@Component({
  selector: 'app-source-chip',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './source-chip.html',
  styleUrl: './source-chip.scss',
})
export class SourceChip {
  readonly domain = input.required<string>();
  readonly href = input.required<string>();
}

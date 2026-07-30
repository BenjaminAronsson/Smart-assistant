import { ChangeDetectionStrategy, Component, input } from '@angular/core';

/**
 * The fallback face for a failure (docs/12 §2.3) — and the `hud-card` switch's
 * own degrade target for an unrecognized card discriminant (docs/12 §9): a
 * card type the client does not register renders this, never raw content.
 * `message` is always plain interpolated text.
 */
@Component({
  selector: 'app-error-card',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './error-card.html',
  styleUrl: './error-card.scss',
  host: {
    role: 'alert',
  },
})
export class ErrorCard {
  readonly message = input.required<string>();
}

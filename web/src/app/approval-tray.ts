import {
  ChangeDetectionStrategy,
  Component,
  computed,
  input,
  output,
  signal,
} from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';
import type { ApprovalCardDto, ApprovalDecisionDto } from '../generated/api-types';

/**
 * ApprovalTray — the human decision surface for an R2/R3 tool proposal (docs/12
 * §2.3 approval card, docs/06 §3). This is the spec's most polished surface: the
 * `exactEffect` string is rendered VERBATIM and never truncated, so the human
 * approves precisely what will run. Editing the arguments rebinds the grant to
 * the edited set (invalidation by hash, docs/06 §4) — so an edit here is a
 * different authorization, never a tweak to the same one.
 *
 * The component is pure: it emits a decision and reflects the `pending`
 * (optimistic-block) state its host passes back. The host owns the POST and
 * removes the card only when the durable `approval.resolved` event confirms it
 * (converge to snapshot truth — angular-shell skill §2/§4).
 */
@Component({
  selector: 'app-approval-tray',
  standalone: true,
  imports: [CommonModule, FormsModule],
  templateUrl: './approval-tray.html',
  styleUrl: './approval-tray.scss',
  changeDetection: ChangeDetectionStrategy.OnPush,
})
export class ApprovalTray {
  /** The pending approval to decide. */
  readonly card = input.required<ApprovalCardDto>();
  /** A decision for this card is in flight — buttons block until it resolves. */
  readonly pending = input(false);

  /** The human's decision, addressed by the card's own ids by the host. */
  readonly decide = output<ApprovalDecisionDto>();

  protected readonly editing = signal(false);
  protected readonly argsDraft = signal('');
  protected readonly argsError = signal<string | null>(null);

  /** The proposed arguments pretty-printed for display/editing (verbatim). */
  protected readonly proposedJson = computed(() =>
    JSON.stringify(this.card().proposedArguments, null, 2),
  );

  /** R2/R3 only ever reach a card (R0/R1 auto-authorize) — label plainly. */
  protected readonly riskLabel = computed(() => this.card().risk.toUpperCase());

  protected startEdit(): void {
    this.argsDraft.set(this.proposedJson());
    this.argsError.set(null);
    this.editing.set(true);
  }

  protected cancelEdit(): void {
    this.editing.set(false);
    this.argsError.set(null);
  }

  protected approve(): void {
    if (this.pending()) return;

    if (!this.editing()) {
      this.decide.emit({ decision: 'approve' });
      return;
    }

    // The editor is open: the edited set rebinds the grant unless it is byte-for-
    // byte the original proposal, in which case the proposal binds (no edit).
    let parsed: unknown;
    try {
      parsed = JSON.parse(this.argsDraft());
    } catch {
      this.argsError.set('Arguments must be valid JSON — the decision was not sent.');
      return;
    }
    const unchanged = JSON.stringify(parsed) === JSON.stringify(this.card().proposedArguments);
    this.decide.emit(
      unchanged ? { decision: 'approve' } : { decision: 'approve', editedArguments: parsed },
    );
  }

  protected deny(): void {
    if (this.pending()) return;
    this.decide.emit({ decision: 'deny' });
  }
}

import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import type { ApprovalCardDto, ApprovalDecisionDto } from '../generated/api-types';
import { ApprovalTray } from './approval-tray';

// A deliberately long, structured effect string: the card must show it VERBATIM,
// so the test asserts the whole thing survives (no truncation) — docs/06 §3.
const EXACT_EFFECT =
  'message.send { to="bob@example.com", subject="Q3 numbers", ' +
  'body="Attached are the final figures for the third quarter review — please confirm." }';

function sampleCard(): ApprovalCardDto {
  return {
    approvalId: '01BX5ZZKBKACTAV9WEVGEMMVRY',
    runId: '01ARZ3NDEKTSV4RRFFQ69G5FAV',
    toolId: 'message.send',
    exactEffect: EXACT_EFFECT,
    proposedArguments: { to: 'bob@example.com', subject: 'Q3 numbers' },
    risk: 'r2',
    reversible: false,
    egress: 'external',
  };
}

describe('ApprovalTray', () => {
  let fixture: ComponentFixture<ApprovalTray>;

  function render(card: ApprovalCardDto, pending = false): HTMLElement {
    fixture = TestBed.createComponent(ApprovalTray);
    fixture.componentRef.setInput('card', card);
    fixture.componentRef.setInput('pending', pending);
    fixture.detectChanges();
    return fixture.nativeElement as HTMLElement;
  }

  beforeEach(() => {
    TestBed.configureTestingModule({
      providers: [provideZonelessChangeDetection()],
    });
  });

  it('renders the exact effect verbatim and untruncated', () => {
    const el = render(sampleCard());
    const effect = el.querySelector('.exact-effect');
    // Byte-for-byte the real target and payload — never a summary or an ellipsis.
    expect(effect?.textContent).toBe(EXACT_EFFECT);
    expect(effect?.textContent).not.toContain('…');
  });

  it('shows the risk, egress, and reversibility so the stakes are legible', () => {
    const el = render(sampleCard());
    expect(el.querySelector('.risk-badge')?.textContent?.trim()).toBe('R2');
    expect(el.querySelector('.egress-badge')?.textContent).toContain('external');
    expect(el.querySelector('.reversible-badge')?.textContent).toContain('not reversible');
  });

  it('is announced as a group labelled by its tool', () => {
    const el = render(sampleCard());
    const group = el.querySelector('[role="group"]');
    expect(group?.getAttribute('aria-label')).toContain('message.send');
  });

  it('approves with no editedArguments when the arguments are untouched', () => {
    const el = render(sampleCard());
    let emitted: ApprovalDecisionDto | undefined;
    fixture.componentInstance.decide.subscribe((d) => (emitted = d));

    el.querySelector<HTMLButtonElement>('button.approve')!.click();

    expect(emitted).toEqual({ decision: 'approve' });
  });

  it('denies with a plain decision', () => {
    const el = render(sampleCard());
    let emitted: ApprovalDecisionDto | undefined;
    fixture.componentInstance.decide.subscribe((d) => (emitted = d));

    el.querySelector<HTMLButtonElement>('button.deny')!.click();

    expect(emitted).toEqual({ decision: 'deny' });
  });

  it('rebinds to the edited arguments when the human changes them', () => {
    const el = render(sampleCard());
    let emitted: ApprovalDecisionDto | undefined;
    fixture.componentInstance.decide.subscribe((d) => (emitted = d));

    el.querySelector<HTMLButtonElement>('button.edit-toggle')!.click();
    fixture.detectChanges();
    const editor = el.querySelector<HTMLTextAreaElement>('textarea.args-editor')!;
    editor.value = '{ "to": "carol@example.com" }';
    editor.dispatchEvent(new Event('input'));
    fixture.detectChanges();

    el.querySelector<HTMLButtonElement>('button.approve')!.click();

    expect(emitted).toEqual({
      decision: 'approve',
      editedArguments: { to: 'carol@example.com' },
    });
  });

  it('does not send a decision when the edited arguments are not valid JSON', () => {
    const el = render(sampleCard());
    let emitted: ApprovalDecisionDto | undefined;
    fixture.componentInstance.decide.subscribe((d) => (emitted = d));

    el.querySelector<HTMLButtonElement>('button.edit-toggle')!.click();
    fixture.detectChanges();
    const editor = el.querySelector<HTMLTextAreaElement>('textarea.args-editor')!;
    editor.value = '{ not json';
    editor.dispatchEvent(new Event('input'));
    fixture.detectChanges();

    el.querySelector<HTMLButtonElement>('button.approve')!.click();
    fixture.detectChanges();

    expect(emitted).toBeUndefined();
    expect(el.querySelector('.args-error')?.textContent).toContain('valid JSON');
  });

  it('blocks the buttons while a decision is in flight (optimistic block)', () => {
    const el = render(sampleCard(), true);
    expect(el.querySelector<HTMLButtonElement>('button.approve')!.disabled).toBeTrue();
    expect(el.querySelector<HTMLButtonElement>('button.deny')!.disabled).toBeTrue();
    expect(el.querySelector('.pending-note')?.textContent).toContain('Sending');
  });
});

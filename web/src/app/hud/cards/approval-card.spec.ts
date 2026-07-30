import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import type { ApprovalCardDto } from '../../../generated/api-types';
import { ApprovalCard } from './approval-card';

describe('ApprovalCard', () => {
  let fixture: ComponentFixture<ApprovalCard>;
  let el: HTMLElement;

  const card: ApprovalCardDto = {
    approvalId: '01BX5ZZKBKACTAV9WEVGEMMVS1',
    runId: '01ARZ3NDEKTSV4RRFFQ69G5FAV',
    toolId: 'message.send',
    exactEffect: 'message.send {to="bob@example.com"}',
    proposedArguments: { to: 'bob@example.com' },
    risk: 'r2',
    reversible: false,
    egress: 'external',
  };

  beforeEach(() => {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(ApprovalCard);
    fixture.componentRef.setInput('card', card);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  });

  it('renders the wire-reused ApprovalTray with the exact effect verbatim', () => {
    const tray = el.querySelector('app-approval-tray');
    expect(tray).not.toBeNull();
    expect(el.textContent).toContain('message.send {to="bob@example.com"}');
  });

  it('forwards the decision emitted by the tray', () => {
    const decisions: unknown[] = [];
    fixture.componentInstance.decide.subscribe((d) => decisions.push(d));
    const approveButton = el.querySelector('.approve') as HTMLButtonElement;
    approveButton.click();
    expect(decisions).toEqual([{ decision: 'approve' }]);
  });
});

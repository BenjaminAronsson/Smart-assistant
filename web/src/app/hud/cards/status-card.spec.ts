import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { StatusCard, type StatusCardData } from './status-card';

describe('StatusCard', () => {
  let fixture: ComponentFixture<StatusCard>;
  let el: HTMLElement;

  function render(card: StatusCardData): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(StatusCard);
    fixture.componentRef.setInput('card', card);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  it('renders the message as plain text and announces as a status region', () => {
    render({
      type: 'card.status',
      id: 'card-7',
      message: 'Queued — provider recovering',
      queued: true,
    });
    expect(el.textContent).toContain('Queued — provider recovering');
    expect(fixture.nativeElement.getAttribute('role')).toBe('status');
  });
});

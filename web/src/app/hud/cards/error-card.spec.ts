import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { ErrorCard } from './error-card';

describe('ErrorCard', () => {
  let fixture: ComponentFixture<ErrorCard>;
  let el: HTMLElement;

  beforeEach(() => {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(ErrorCard);
    fixture.componentRef.setInput('message', 'Could not load this result');
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  });

  it('renders the message as plain text and announces itself', () => {
    expect(el.textContent).toContain('Could not load this result');
    expect(fixture.nativeElement.getAttribute('role')).toBe('alert');
  });
});

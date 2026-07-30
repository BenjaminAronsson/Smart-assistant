import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { SourceChip } from './source-chip';

describe('SourceChip', () => {
  let fixture: ComponentFixture<SourceChip>;
  let el: HTMLElement;

  beforeEach(() => {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(SourceChip);
    fixture.componentRef.setInput('domain', 'wikipedia.org');
    fixture.componentRef.setInput('href', 'https://en.wikipedia.org/wiki/Ramen');
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  });

  it('shows the display domain as visible text', () => {
    expect(el.textContent).toContain('wikipedia.org');
  });

  it('is a real, keyboard-reachable link to the source page', () => {
    const anchor = el.querySelector('a') as HTMLAnchorElement;
    expect(anchor).not.toBeNull();
    expect(anchor.getAttribute('href')).toBe('https://en.wikipedia.org/wiki/Ramen');
    expect(anchor.tabIndex).not.toBe(-1);
  });

  it('opens the source in a new tab without leaking a referrer or opener', () => {
    const anchor = el.querySelector('a') as HTMLAnchorElement;
    expect(anchor.getAttribute('rel')).toContain('noopener');
    expect(anchor.getAttribute('referrerpolicy')).toBe('no-referrer');
  });
});

import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { UnsupportedRenderer } from './unsupported-renderer';

describe('UnsupportedRenderer', () => {
  let fixture: ComponentFixture<UnsupportedRenderer>;
  let el: HTMLElement;

  function render(kind: string): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(UnsupportedRenderer);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    fixture.componentRef.setInput('kind', kind as any);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  it('explicitly names a bundle artifact as unsupported here rather than a blank panel', () => {
    render('bundle');
    expect(el.textContent).toContain('Generated app (bundle)');
    expect(el.textContent).toContain('sandbox');
  });

  it('degrades an unknown/future kind to a message instead of a blank panel', () => {
    render('something_new_from_the_future');
    expect(el.querySelector('.unsupported')).not.toBeNull();
    expect(el.textContent?.trim().length).toBeGreaterThan(0);
  });
});

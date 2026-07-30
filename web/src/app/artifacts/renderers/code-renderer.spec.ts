import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { CodeRenderer } from './code-renderer';

describe('CodeRenderer', () => {
  let fixture: ComponentFixture<CodeRenderer>;
  let el: HTMLElement;

  function render(content: string, mediaType: string | null = null): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(CodeRenderer);
    fixture.componentRef.setInput('content', content);
    fixture.componentRef.setInput('mediaType', mediaType);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  it('renders text content verbatim inside <pre><code>', () => {
    render('fn main() {\n    println!("hi");\n}');
    expect(el.querySelector('pre code')?.textContent).toBe('fn main() {\n    println!("hi");\n}');
  });

  it('shows the media type label when provided', () => {
    render('diff --git a b', 'text/x-diff');
    expect(el.querySelector('.media-type')?.textContent).toContain('text/x-diff');
  });

  it('never creates a script element even if the text looks like markup', () => {
    render('<script>alert(1)</script>');
    expect(el.querySelector('script')).toBeNull();
    expect(el.querySelector('pre code')?.textContent).toBe('<script>alert(1)</script>');
  });
});

import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { MarkdownRenderer } from './markdown-renderer';

describe('MarkdownRenderer', () => {
  let fixture: ComponentFixture<MarkdownRenderer>;
  let el: HTMLElement;

  function render(content: string): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(MarkdownRenderer);
    fixture.componentRef.setInput('content', content);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  it('renders headings, paragraphs, lists and code blocks', () => {
    render('# Title\n\nA paragraph.\n\n- one\n- two\n\n```\ncode here\n```');
    expect(el.querySelector('h1')?.textContent).toContain('Title');
    expect(el.querySelector('p')?.textContent).toContain('A paragraph.');
    expect(el.querySelectorAll('li').length).toBe(2);
    expect(el.querySelector('pre code')?.textContent).toContain('code here');
  });

  it('renders a safe link as a real, sanctioned anchor', () => {
    render('[docs](https://example.com)');
    const anchor = el.querySelector('a');
    expect(anchor?.getAttribute('href')).toBe('https://example.com');
    expect(anchor?.getAttribute('rel')).toContain('noopener');
  });

  // --- Threat note: a malicious artifact must render as inert text. ---

  it('never creates a <script> element from artifact bytes', () => {
    render('Look at this: <script>window.pwned = true</script>');
    expect(el.querySelector('script')).toBeNull();
    expect(el.textContent).toContain('<script>window.pwned = true</script>');
    expect((window as unknown as { pwned?: boolean }).pwned).toBeUndefined();
  });

  it('never creates an <img> element or fires onerror from artifact bytes', () => {
    render('<img src=x onerror=window.pwned=true>');
    expect(el.querySelector('img')).toBeNull();
    expect(el.textContent).toContain('onerror=window.pwned=true');
  });

  it('never binds a javascript: URL as a navigable href', () => {
    render('[click me](javascript:window.pwned=true)');
    const anchors = Array.from(el.querySelectorAll('a'));
    expect(anchors.every((a) => !a.getAttribute('href')?.startsWith('javascript:'))).toBeTrue();
    expect(el.textContent).toContain('click me');
  });

  it('never creates an element carrying an onclick handler from artifact bytes', () => {
    render('<div onclick="window.pwned=true">text</div>');
    for (const node of Array.from(el.querySelectorAll('*'))) {
      expect(node.getAttribute('onclick')).toBeNull();
    }
    expect(el.textContent).toContain('onclick="window.pwned=true"');
  });

  it('shows an explicit empty state rather than a blank panel', () => {
    render('');
    expect(el.querySelector('.empty')?.textContent).toContain('no content');
  });
});

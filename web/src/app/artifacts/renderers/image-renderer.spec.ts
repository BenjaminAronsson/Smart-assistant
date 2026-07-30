import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { ImageRenderer } from './image-renderer';

describe('ImageRenderer', () => {
  let fixture: ComponentFixture<ImageRenderer>;
  let el: HTMLElement;

  beforeEach(() => {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(ImageRenderer);
    el = fixture.nativeElement as HTMLElement;
  });

  it('renders the blob as an <img> via a blob: object URL', () => {
    fixture.componentRef.setInput('blob', new Blob(['fake-bytes'], { type: 'image/png' }));
    fixture.componentRef.setInput('label', 'A picture');
    fixture.detectChanges();

    const img = el.querySelector('img');
    expect(img?.src.startsWith('blob:')).toBeTrue();
    expect(img?.alt).toBe('A picture');
  });

  it('never uses <object>, <embed> or <iframe> to display the blob', () => {
    fixture.componentRef.setInput('blob', new Blob(['<svg onload="x()">'], { type: 'image/svg+xml' }));
    fixture.detectChanges();

    expect(el.querySelector('object')).toBeNull();
    expect(el.querySelector('embed')).toBeNull();
    expect(el.querySelector('iframe')).toBeNull();
  });

  it('revokes the previous object URL when the blob changes, and on destroy', () => {
    const revokeSpy = spyOn(URL, 'revokeObjectURL').and.callThrough();
    fixture.componentRef.setInput('blob', new Blob(['one'], { type: 'image/png' }));
    fixture.detectChanges();
    const firstUrl = (el.querySelector('img') as HTMLImageElement).src;

    fixture.componentRef.setInput('blob', new Blob(['two'], { type: 'image/png' }));
    fixture.detectChanges();
    expect(revokeSpy).toHaveBeenCalledWith(firstUrl);

    fixture.destroy();
    expect(revokeSpy).toHaveBeenCalledTimes(2);
  });
});

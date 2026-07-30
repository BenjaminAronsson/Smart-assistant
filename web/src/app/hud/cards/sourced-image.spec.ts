import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import type { SourcedImageDto } from '../../../generated/api-types';
import { SourcedImage } from './sourced-image';

describe('SourcedImage', () => {
  let fixture: ComponentFixture<SourcedImage>;
  let el: HTMLElement;

  const photo: SourcedImageDto = {
    url: 'https://cdn.example/ramen.jpg',
    sourceUrl: 'https://en.wikipedia.org/wiki/Ramen',
    sourceDomain: 'wikipedia.org',
    alt: 'A bowl of ramen',
  };

  beforeEach(() => {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(SourcedImage);
    fixture.componentRef.setInput('image', photo);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  });

  it('renders the image with its required alt text', () => {
    const img = el.querySelector('img') as HTMLImageElement;
    expect(img.getAttribute('src')).toBe(photo.url);
    expect(img.getAttribute('alt')).toBe(photo.alt);
  });

  // docs/12 §2.3 / §9 acceptance: every web-sourced image shows its source link.
  it('always renders the source-link chip alongside the image, showing the domain', () => {
    const chip = el.querySelector('app-source-chip') as HTMLElement;
    expect(chip).not.toBeNull();
    expect(chip.textContent).toContain('wikipedia.org');
  });
});

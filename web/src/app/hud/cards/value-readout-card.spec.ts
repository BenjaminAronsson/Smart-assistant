import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { ValueReadoutCard, type ValueReadoutCardData } from './value-readout-card';

describe('ValueReadoutCard', () => {
  let fixture: ComponentFixture<ValueReadoutCard>;
  let el: HTMLElement;

  const card: ValueReadoutCardData = {
    type: 'card.value_readout',
    id: 'card-1',
    label: 'Weather in Berlin',
    value: '72°F',
    miniStats: [{ label: 'Humidity', value: '68%' }],
  };

  beforeEach(() => {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(ValueReadoutCard);
    fixture.componentRef.setInput('card', card);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  });

  it('renders the hero value and label as plain text', () => {
    expect(el.textContent).toContain('72°F');
    expect(el.textContent).toContain('Weather in Berlin');
  });

  it('renders every mini-stat', () => {
    expect(el.textContent).toContain('Humidity');
    expect(el.textContent).toContain('68%');
  });

  it('renders no image or source chip when the readout carries none', () => {
    expect(el.querySelector('app-sourced-image')).toBeNull();
    expect(el.querySelector('app-source-chip')).toBeNull();
  });
});

import { provideZonelessChangeDetection } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { ChartRenderer } from './chart-renderer';

describe('ChartRenderer', () => {
  let fixture: ComponentFixture<ChartRenderer>;
  let el: HTMLElement;

  function render(content: string): void {
    TestBed.configureTestingModule({ providers: [provideZonelessChangeDetection()] });
    fixture = TestBed.createComponent(ChartRenderer);
    fixture.componentRef.setInput('content', content);
    fixture.detectChanges();
    el = fixture.nativeElement as HTMLElement;
  }

  it('renders a bar per category for a single-series chart, with no legend', () => {
    render(
      JSON.stringify({
        title: 'Requests/day',
        series: [
          {
            name: 'requests',
            points: [
              { label: 'Mon', value: 10 },
              { label: 'Tue', value: 14 },
              { label: 'Wed', value: 3 },
            ],
          },
        ],
      }),
    );
    expect(el.querySelectorAll('svg rect').length).toBe(3);
    expect(el.querySelector('.legend')).toBeNull();
    expect(el.querySelector('.chart-title')?.textContent).toContain('Requests/day');
  });

  it('shows a legend for multi-series charts and never colors text by series', () => {
    render(
      JSON.stringify({
        series: [
          { name: 'a', points: [{ label: 'x', value: 1 }] },
          { name: 'b', points: [{ label: 'x', value: 2 }] },
        ],
      }),
    );
    const legendItems = el.querySelectorAll('.legend li');
    expect(legendItems.length).toBe(2);
    expect(legendItems[0].textContent).toContain('a');
    // Text nodes carry no inline series color — only the swatch span does.
    const swatch = el.querySelector('.swatch') as HTMLElement;
    expect(swatch.style.background).toContain('var(--series-1)');
  });

  it('caps rendered series at 4 and notes the rest', () => {
    const series = Array.from({ length: 6 }, (_, i) => ({
      name: `s${i}`,
      points: [{ label: 'x', value: i }],
    }));
    render(JSON.stringify({ series }));
    expect(el.querySelectorAll('.legend li').length).toBe(4);
    expect(el.querySelector('.hidden-note')?.textContent).toContain('2 more');
  });

  it('renders a line chart as a path plus point markers', () => {
    render(
      JSON.stringify({
        chartType: 'line',
        series: [
          {
            name: 'temp',
            points: [
              { label: '0', value: 1 },
              { label: '1', value: 5 },
            ],
          },
        ],
      }),
    );
    expect(el.querySelector('.line-path')).not.toBeNull();
    expect(el.querySelectorAll('svg circle').length).toBe(2);
  });

  it('shows an explicit invalid-data message rather than a blank panel', () => {
    render('not valid json');
    expect(el.querySelector('.invalid')?.textContent).toContain('invalid');
    expect(el.querySelector('svg')).toBeNull();
  });

  it('toggles between the chart and an accessible data table', () => {
    render(JSON.stringify({ series: [{ name: 'a', points: [{ label: 'x', value: 1 }] }] }));
    expect(el.querySelector('table')).toBeNull();
    (el.querySelector('.table-toggle') as HTMLButtonElement).click();
    fixture.detectChanges();
    expect(el.querySelector('table')).not.toBeNull();
    expect(el.querySelector('svg')).toBeNull();
  });
});

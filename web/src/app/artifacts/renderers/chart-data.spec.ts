import { parseChartData } from './chart-data';

describe('parseChartData', () => {
  it('parses a valid single-series bar chart', () => {
    const data = parseChartData(
      JSON.stringify({
        title: 'Requests',
        series: [
          {
            name: 'requests',
            points: [
              { label: 'Mon', value: 10 },
              { label: 'Tue', value: 14 },
            ],
          },
        ],
      }),
    );
    expect(data).not.toBeNull();
    expect(data?.chartType).toBe('bar');
    expect(data?.series.length).toBe(1);
    expect(data?.series[0].points.length).toBe(2);
  });

  it('defaults chartType to bar and accepts an explicit line', () => {
    const line = parseChartData(
      JSON.stringify({ chartType: 'line', series: [{ name: 'a', points: [{ label: 'x', value: 1 }] }] }),
    );
    expect(line?.chartType).toBe('line');
  });

  it('rejects malformed JSON rather than throwing', () => {
    expect(parseChartData('{not json')).toBeNull();
  });

  it('rejects JSON that is not an object', () => {
    expect(parseChartData('42')).toBeNull();
    expect(parseChartData('null')).toBeNull();
    expect(parseChartData('[]')).toBeNull();
  });

  it('rejects a missing or empty series array', () => {
    expect(parseChartData(JSON.stringify({}))).toBeNull();
    expect(parseChartData(JSON.stringify({ series: [] }))).toBeNull();
  });

  it('rejects a series with a non-numeric point value', () => {
    const data = parseChartData(
      JSON.stringify({ series: [{ name: 'a', points: [{ label: 'x', value: 'not-a-number' }] }] }),
    );
    expect(data).toBeNull();
  });

  it('rejects a point missing its label', () => {
    const data = parseChartData(JSON.stringify({ series: [{ name: 'a', points: [{ value: 1 }] }] }));
    expect(data).toBeNull();
  });

  it('rejects NaN/Infinity values', () => {
    const data = parseChartData(
      JSON.stringify({ series: [{ name: 'a', points: [{ label: 'x', value: Infinity }] }] }),
    );
    // JSON.stringify turns Infinity into null, which then fails the numeric check.
    expect(data).toBeNull();
  });
});

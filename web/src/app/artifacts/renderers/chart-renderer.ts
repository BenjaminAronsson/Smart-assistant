import { ChangeDetectionStrategy, Component, computed, input, signal } from '@angular/core';
import { CHART_SERIES_CAP, type ChartArtifactData, parseChartData } from './chart-data';

/** One drawn bar (viewBox units, not pixels — the SVG scales with its
 * container per docs/12 §7's resolution-independence rule; the dataviz
 * skill's 2px gap/rounding specs are followed as *relative* proportions of
 * the drawn geometry instead of literal pixels, since a literal px value
 * would not survive scaling to a 4K canvas or a 390px-wide one). */
interface Bar {
  x: number;
  y: number;
  width: number;
  height: number;
  rx: number;
  seriesIndex: number;
  category: string;
  seriesName: string;
  value: number;
}

interface LinePoint {
  x: number;
  y: number;
  category: string;
  value: number;
}

interface LineSeries {
  seriesIndex: number;
  seriesName: string;
  d: string;
  points: LinePoint[];
}

const VIEW_W = 100;
const VIEW_H = 60;
const LEFT_MARGIN = 4;
const RIGHT_MARGIN = 4;
const PLOT_TOP = 4;
const PLOT_BOTTOM = 48;

/**
 * Simple chart artifact renderer (`ArtifactKindDto.chart`, dataviz skill).
 * The blob is untrusted JSON (F3b.3 threat note) parsed defensively by
 * {@link parseChartData} — malformed data shows an explicit message, never a
 * crash or a blank panel. Geometry is computed here and bound through
 * attribute bindings only; there is no `[innerHTML]` anywhere in this chart.
 *
 * Categorical color assignment is fixed-order (never cycled) and capped at
 * {@link CHART_SERIES_CAP} series — a 5th series folds into "+N more" rather
 * than growing the ramp (dataviz skill: color formula check 4). Colors are
 * CSS custom properties (`--series-N`) so light/dark swap in one place
 * (dataviz skill `references/palette.md`), not by re-computing hex in JS.
 */
@Component({
  selector: 'app-chart-renderer',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './chart-renderer.html',
  styleUrl: './chart-renderer.scss',
})
export class ChartRenderer {
  readonly content = input.required<string>();

  protected readonly viewBox = `0 0 ${VIEW_W} ${VIEW_H}`;
  protected readonly showTable = signal(false);

  protected readonly data = computed<ChartArtifactData | null>(() => parseChartData(this.content()));

  protected readonly renderedSeries = computed(
    () => this.data()?.series.slice(0, CHART_SERIES_CAP) ?? [],
  );
  protected readonly hiddenSeriesCount = computed(() =>
    Math.max(0, (this.data()?.series.length ?? 0) - CHART_SERIES_CAP),
  );
  protected readonly showLegend = computed(() => this.renderedSeries().length > 1);

  /** Category axis: the longest rendered series' labels (docs: a "simple
   * chart" — series of different lengths line up by index, not by relabeling). */
  protected readonly categories = computed<string[]>(() => {
    const series = this.renderedSeries();
    let longest: string[] = [];
    for (const s of series) {
      if (s.points.length > longest.length) longest = s.points.map((p) => p.label);
    }
    return longest;
  });

  private readonly domain = computed<[number, number]>(() => {
    const values = this.renderedSeries().flatMap((s) => s.points.map((p) => p.value));
    const min = Math.min(0, ...values);
    const max = Math.max(0, ...values);
    return min === max ? [min, max + 1] : [min, max];
  });

  private mapValue(v: number): number {
    const [min, max] = this.domain();
    const t = (v - min) / (max - min);
    return PLOT_BOTTOM - t * (PLOT_BOTTOM - PLOT_TOP);
  }

  protected readonly baselineY = computed(() => this.mapValue(0));

  /** Whether there are few enough marks to label every one directly (dataviz
   * skill: "selective direct labels, never a number on every point"). Above
   * this the table-view toggle is the accessible way to read exact values. */
  protected readonly canDirectLabel = computed(
    () => this.categories().length * this.renderedSeries().length <= 12,
  );

  protected readonly bars = computed<Bar[]>(() => {
    if (this.data()?.chartType !== 'bar') return [];
    const categories = this.categories();
    const series = this.renderedSeries();
    if (categories.length === 0 || series.length === 0) return [];

    const plotWidth = VIEW_W - LEFT_MARGIN - RIGHT_MARGIN;
    const groupWidth = plotWidth / categories.length;
    const gap = groupWidth * 0.06;
    const barWidth = (groupWidth - gap * (series.length + 1)) / series.length;
    const rx = Math.min(1.2, barWidth / 3);
    const baseline = this.baselineY();

    const out: Bar[] = [];
    categories.forEach((category, i) => {
      series.forEach((s, j) => {
        const value = s.points[i]?.value ?? 0;
        const y = this.mapValue(value);
        const top = Math.min(y, baseline);
        const x = LEFT_MARGIN + groupWidth * i + gap * (j + 1) + barWidth * j;
        out.push({
          x,
          y: top,
          width: barWidth,
          height: Math.max(0, Math.abs(y - baseline)),
          rx,
          seriesIndex: j,
          category,
          seriesName: s.name,
          value,
        });
      });
    });
    return out;
  });

  protected readonly lines = computed<LineSeries[]>(() => {
    if (this.data()?.chartType !== 'line') return [];
    const categories = this.categories();
    const series = this.renderedSeries();
    if (categories.length === 0 || series.length === 0) return [];

    const plotWidth = VIEW_W - LEFT_MARGIN - RIGHT_MARGIN;
    const step = categories.length > 1 ? plotWidth / (categories.length - 1) : 0;
    const centerX = (i: number): number =>
      categories.length > 1 ? LEFT_MARGIN + step * i : LEFT_MARGIN + plotWidth / 2;

    return series.map((s, seriesIndex) => {
      const points: LinePoint[] = categories.map((category, i) => ({
        x: centerX(i),
        y: this.mapValue(s.points[i]?.value ?? 0),
        category,
        value: s.points[i]?.value ?? 0,
      }));
      const d = points.map((p, i) => `${i === 0 ? 'M' : 'L'}${p.x},${p.y}`).join(' ');
      return { seriesIndex, seriesName: s.name, d, points };
    });
  });

  protected readonly categoryTickX = computed<number[]>(() => {
    const categories = this.categories();
    const plotWidth = VIEW_W - LEFT_MARGIN - RIGHT_MARGIN;
    if (this.data()?.chartType === 'line') {
      const step = categories.length > 1 ? plotWidth / (categories.length - 1) : 0;
      return categories.map((_, i) => (categories.length > 1 ? LEFT_MARGIN + step * i : LEFT_MARGIN + plotWidth / 2));
    }
    const groupWidth = plotWidth / Math.max(1, categories.length);
    return categories.map((_, i) => LEFT_MARGIN + groupWidth * (i + 0.5));
  });

  protected toggleTable(): void {
    this.showTable.update((v) => !v);
  }

  protected trackByIndex(index: number): number {
    return index;
  }
}

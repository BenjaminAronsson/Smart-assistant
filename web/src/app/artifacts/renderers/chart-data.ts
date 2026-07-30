/**
 * The JSON shape a `chart` artifact's blob carries (docs/02 §6: "simple
 * charts"). There is no contracts DTO for this yet — a chart artifact's blob
 * is the model's own JSON output, not a jarvisd wire response — so this is a
 * small, closed, defensively-parsed shape owned by the renderer, not a
 * generated type. `parseChartData` never throws: malformed or hostile JSON
 * (artifact bytes are untrusted, F3b.3 threat note) becomes `null`, and the
 * renderer shows an explicit "invalid chart data" message rather than
 * crashing or going blank.
 */
export interface ChartPoint {
  label: string;
  value: number;
}

export interface ChartSeries {
  name: string;
  points: ChartPoint[];
}

export interface ChartArtifactData {
  chartType: 'bar' | 'line';
  title: string | null;
  unit: string | null;
  series: ChartSeries[];
}

/** Categorical fixed-order slots this renderer draws from — never cycled, in
 * this exact order (dataviz skill: "assign categorical hues in fixed order").
 * Capped at 4 series: the reference palette validates the full 8-hue set for
 * *adjacent* pairs (bars/lines), and 4 is already generous for a "simple
 * chart" — a 5th series folds into "+N more" rather than growing the ramp. */
export const CHART_SERIES_CAP = 4;

function isChartPoint(v: unknown): v is ChartPoint {
  if (typeof v !== 'object' || v === null) return false;
  const o = v as Record<string, unknown>;
  return typeof o['label'] === 'string' && typeof o['value'] === 'number' && Number.isFinite(o['value']);
}

function isChartSeries(v: unknown): v is ChartSeries {
  if (typeof v !== 'object' || v === null) return false;
  const o = v as Record<string, unknown>;
  return typeof o['name'] === 'string' && Array.isArray(o['points']) && o['points'].every(isChartPoint);
}

/** Parses and validates chart artifact JSON text. Returns `null` on any
 * malformed shape — never throws, never partially trusts the input. */
export function parseChartData(text: string): ChartArtifactData | null {
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return null;
  }
  if (typeof raw !== 'object' || raw === null) return null;
  const o = raw as Record<string, unknown>;

  if (!Array.isArray(o['series']) || o['series'].length === 0 || !o['series'].every(isChartSeries)) {
    return null;
  }
  const chartType = o['chartType'] === 'line' ? 'line' : 'bar';
  const title = typeof o['title'] === 'string' ? o['title'] : null;
  const unit = typeof o['unit'] === 'string' ? o['unit'] : null;

  return { chartType, title, unit, series: o['series'] as ChartSeries[] };
}

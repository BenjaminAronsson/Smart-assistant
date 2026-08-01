import type { MapBoundsDto, MapPointDto } from '../../../generated/api-types';

/**
 * Geometry and render-mode decisions for the map card (F3b.5, docs/12 §3,
 * ADR-013). Pure, dependency-free functions — no MapLibre import here, so
 * this module (and the mode decision it drives) is reachable without ever
 * pulling the heavy GL bundle in (same separation as `chart-data.ts`'s
 * `parseChartData`: the decision logic is unit-testable on its own).
 */

/**
 * Whether `point` falls inside `bounds` (docs/12 §3's "coverage"). `bounds`
 * is authoritative from the coverage endpoint, but this is only a client-side
 * pre-filter for *which renderer to pick* — the server still refuses any
 * individual out-of-region tile regardless of what this concludes (jarvisd's
 * `Archive::covers`), so a wrong guess here degrades to an empty tile, never
 * a wrong-region one.
 *
 * Handles an antimeridian-crossing bbox (`minLon > maxLon`) defensively, even
 * though a home-region extract is very unlikely to cross it.
 */
export function pointInBounds(point: MapPointDto, bounds: MapBoundsDto): boolean {
  const { minLon, minLat, maxLon, maxLat } = bounds;
  if (point.lat < minLat || point.lat > maxLat) return false;
  if (minLon <= maxLon) {
    return point.lon >= minLon && point.lon <= maxLon;
  }
  // Wrapped range: covers [minLon, 180] ∪ [-180, maxLon].
  return point.lon >= minLon || point.lon <= maxLon;
}

/** What the coverage lookup concluded, kept distinct from a render mode so
 * the decision function stays a pure, exhaustively-testable `switch`. */
export type MapCoverageResult =
  | { kind: 'available'; bounds: MapBoundsDto }
  /** `GET /api/v1/map/coverage` 404'd: no archive is configured at all
   * (docs/09 §1 — absent, not broken). */
  | { kind: 'unconfigured' }
  /** The request itself failed (network error, 5xx) — we could not learn
   * whether an archive exists, so this is treated the same as "not covered"
   * rather than guessed at (docs/12 §3: never show the wrong place). */
  | { kind: 'unknown' };

/** The three faces docs/12 §3 requires: the real local map, the online
 * fallback, or the offline coordinates-only degrade. Never a fourth "blank"
 * state — every `MapCoverageResult` × online combination maps to one of
 * these three. */
export type MapRenderMode = 'local' | 'raster' | 'coords';

/**
 * The coverage-fallback decision (docs/12 §3): local PMTiles when the
 * destination is inside the served archive's bounds, else online OSM raster
 * if the network is up, else a coordinates-only card. `online` is the
 * caller's best current signal (`navigator.onLine`) — a heuristic, but the
 * failure mode of guessing wrong is "raster tiles fail to load", not a wrong
 * or blank map, so a heuristic is an acceptable input here.
 */
export function decideMapRenderMode(
  coverage: MapCoverageResult,
  destination: MapPointDto,
  online: boolean,
): MapRenderMode {
  if (coverage.kind === 'available' && pointInBounds(destination, coverage.bounds)) {
    return 'local';
  }
  return online ? 'raster' : 'coords';
}

const RAD = Math.PI / 180;

/** Initial great-circle bearing from `from` to `to`, in degrees `[0, 360)`. */
export function initialBearingDeg(from: MapPointDto, to: MapPointDto): number {
  const phi1 = from.lat * RAD;
  const phi2 = to.lat * RAD;
  const deltaLambda = (to.lon - from.lon) * RAD;
  const y = Math.sin(deltaLambda) * Math.cos(phi2);
  const x = Math.cos(phi1) * Math.sin(phi2) - Math.sin(phi1) * Math.cos(phi2) * Math.cos(deltaLambda);
  const theta = Math.atan2(y, x);
  return ((theta / RAD) % 360 + 360) % 360;
}

const COMPASS_POINTS = ['N', 'NE', 'E', 'SE', 'S', 'SW', 'W', 'NW'] as const;

/** `bearingDeg` reduced to one of 8 compass points, for a plain-language
 * "bearing from home" readout (docs/12 §3's coordinates-only card). */
export function compassLabel(bearingDeg: number): string {
  const index = Math.round(bearingDeg / 45) % 8;
  return COMPASS_POINTS[index];
}

/**
 * The smallest bbox covering every given point, as `[[minLon, minLat],
 * [maxLon, maxLat]]` (MapLibre's `fitBounds` shape). Used to frame the
 * destination together with the current-location dot and route, in both
 * local and raster render modes, instead of a hardcoded zoom that would
 * clip one of them. Returns `null` for an empty list — callers fall back to
 * centering on the destination alone.
 */
export function computeFitBounds(points: MapPointDto[]): [[number, number], [number, number]] | null {
  if (points.length === 0) return null;
  let minLon = points[0].lon;
  let maxLon = points[0].lon;
  let minLat = points[0].lat;
  let maxLat = points[0].lat;
  for (const p of points) {
    if (p.lon < minLon) minLon = p.lon;
    if (p.lon > maxLon) maxLon = p.lon;
    if (p.lat < minLat) minLat = p.lat;
    if (p.lat > maxLat) maxLat = p.lat;
  }
  return [
    [minLon, minLat],
    [maxLon, maxLat],
  ];
}

/** `37.7749° N, 122.4194° W` — a fixed, locale-independent coordinate
 * readout (tabular-nums applies in CSS; this only decides sign→hemisphere
 * and rounding). */
export function formatCoordinate(point: MapPointDto): string {
  const lat = Math.abs(point.lat).toFixed(4);
  const lon = Math.abs(point.lon).toFixed(4);
  const ns = point.lat >= 0 ? 'N' : 'S';
  const ew = point.lon >= 0 ? 'E' : 'W';
  return `${lat}° ${ns}, ${lon}° ${ew}`;
}

import {
  compassLabel,
  computeFitBounds,
  decideMapRenderMode,
  formatCoordinate,
  initialBearingDeg,
  pointInBounds,
} from './map-geo';

const SF_BOUNDS = { minLon: -122.6, minLat: 37.6, maxLon: -122.3, maxLat: 37.85 };

describe('pointInBounds', () => {
  it('is true for a point inside a normal (non-wrapped) bbox', () => {
    expect(pointInBounds({ lon: -122.4194, lat: 37.7749 }, SF_BOUNDS)).toBe(true);
  });

  it('is false outside latitude range', () => {
    expect(pointInBounds({ lon: -122.4194, lat: 40 }, SF_BOUNDS)).toBe(false);
  });

  it('is false outside longitude range', () => {
    expect(pointInBounds({ lon: -100, lat: 37.7 }, SF_BOUNDS)).toBe(false);
  });

  it('handles an antimeridian-crossing bbox', () => {
    const wrapped = { minLon: 170, minLat: -10, maxLon: -170, maxLat: 10 };
    expect(pointInBounds({ lon: 179, lat: 0 }, wrapped)).toBe(true);
    expect(pointInBounds({ lon: -179, lat: 0 }, wrapped)).toBe(true);
    expect(pointInBounds({ lon: 0, lat: 0 }, wrapped)).toBe(false);
  });

  it('includes the boundary itself', () => {
    expect(pointInBounds({ lon: SF_BOUNDS.minLon, lat: SF_BOUNDS.minLat }, SF_BOUNDS)).toBe(true);
    expect(pointInBounds({ lon: SF_BOUNDS.maxLon, lat: SF_BOUNDS.maxLat }, SF_BOUNDS)).toBe(true);
  });
});

describe('decideMapRenderMode (docs/12 §3 coverage fallback)', () => {
  const inRegion = { lon: -122.4194, lat: 37.7749 };
  const angkorWat = { lon: 103.866667, lat: 13.4125 };

  it('picks local when the archive covers the destination', () => {
    const mode = decideMapRenderMode({ kind: 'available', bounds: SF_BOUNDS }, inRegion, true);
    expect(mode).toBe('local');
  });

  it('picks local even offline, when the archive covers the destination (no network needed)', () => {
    const mode = decideMapRenderMode({ kind: 'available', bounds: SF_BOUNDS }, inRegion, false);
    expect(mode).toBe('local');
  });

  it('falls back to online raster when out of coverage and online', () => {
    const mode = decideMapRenderMode({ kind: 'available', bounds: SF_BOUNDS }, angkorWat, true);
    expect(mode).toBe('raster');
  });

  it('falls back to coordinates-only when out of coverage and offline — never blank', () => {
    const mode = decideMapRenderMode({ kind: 'available', bounds: SF_BOUNDS }, angkorWat, false);
    expect(mode).toBe('coords');
  });

  it('with no archive configured at all, still falls back correctly online/offline', () => {
    expect(decideMapRenderMode({ kind: 'unconfigured' }, angkorWat, true)).toBe('raster');
    expect(decideMapRenderMode({ kind: 'unconfigured' }, angkorWat, false)).toBe('coords');
  });

  it('treats a failed coverage lookup as "not covered", never as local', () => {
    expect(decideMapRenderMode({ kind: 'unknown' }, inRegion, true)).toBe('raster');
    expect(decideMapRenderMode({ kind: 'unknown' }, inRegion, false)).toBe('coords');
  });
});

describe('initialBearingDeg / compassLabel', () => {
  it('due north is ~0deg', () => {
    const bearing = initialBearingDeg({ lon: 0, lat: 0 }, { lon: 0, lat: 10 });
    expect(bearing).toBeCloseTo(0, 0);
    expect(compassLabel(bearing)).toBe('N');
  });

  it('due east is ~90deg', () => {
    const bearing = initialBearingDeg({ lon: 0, lat: 0 }, { lon: 10, lat: 0 });
    expect(bearing).toBeCloseTo(90, 0);
    expect(compassLabel(bearing)).toBe('E');
  });

  it('wraps 360 back to N', () => {
    expect(compassLabel(359)).toBe('N');
    expect(compassLabel(0)).toBe('N');
  });
});

describe('computeFitBounds', () => {
  it('returns null for an empty list', () => {
    expect(computeFitBounds([])).toBeNull();
  });

  it('collapses to a point bbox for a single point', () => {
    const p = { lon: 1, lat: 2 };
    expect(computeFitBounds([p])).toEqual([
      [1, 2],
      [1, 2],
    ]);
  });

  it('covers every point across a set', () => {
    const points = [
      { lon: -122.42, lat: 37.77 },
      { lon: -122.4194, lat: 37.7749 },
      { lon: -122.5, lat: 37.6 },
    ];
    expect(computeFitBounds(points)).toEqual([
      [-122.5, 37.6],
      [-122.4194, 37.7749],
    ]);
  });
});

describe('formatCoordinate', () => {
  it('renders hemisphere letters from sign', () => {
    expect(formatCoordinate({ lon: -122.4194, lat: 37.7749 })).toBe('37.7749° N, 122.4194° W');
    expect(formatCoordinate({ lon: 103.866667, lat: -13.4125 })).toBe('13.4125° S, 103.8667° E');
  });
});

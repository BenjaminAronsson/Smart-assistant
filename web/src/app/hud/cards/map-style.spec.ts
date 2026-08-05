import { buildMapStyle } from './map-style';

describe('buildMapStyle', () => {
  const base = {
    tileUrlTemplate: '/api/v1/map/tiles/{z}/{x}/{y}',
    minZoom: 0,
    maxZoom: 14,
  };

  it('builds a single-layer raster style for a raster tile format', () => {
    const style = buildMapStyle({ ...base, mode: 'local', tileFormat: 'png' });
    expect(style.version).toBe(8);
    const sourceIds = Object.keys(style.sources);
    expect(sourceIds.length).toBe(1);
    const source = style.sources[sourceIds[0]];
    expect(source.type).toBe('raster');
    expect((source as { tiles: string[] }).tiles).toEqual([base.tileUrlTemplate]);
    expect(style.layers.every((l) => l.type === 'raster')).toBe(true);
  });

  it('builds the same raster shape for the online fallback (raster is raster regardless of source)', () => {
    const local = buildMapStyle({ ...base, mode: 'local', tileFormat: 'jpeg' });
    const raster = buildMapStyle({ ...base, mode: 'raster', tileFormat: 'jpeg' });
    expect(local.layers.map((l) => l.type)).toEqual(raster.layers.map((l) => l.type));
  });

  it('builds a vector style with named source-layers for mvt', () => {
    const style = buildMapStyle({ ...base, mode: 'local', tileFormat: 'mvt' });
    const sourceIds = Object.keys(style.sources);
    expect(style.sources[sourceIds[0]].type).toBe('vector');
    const sourceLayers = style.layers
      .filter((l): l is typeof l & { 'source-layer': string } => 'source-layer' in l)
      .map((l) => l['source-layer']);
    expect(sourceLayers).toEqual(
      jasmine.arrayContaining(['earth', 'water', 'landuse', 'buildings', 'roads', 'boundaries']),
    );
  });

  it('never renders zero layers, even for an unrecognized tile format value', () => {
    // Defensive: the wire enum may grow (`avif`/`webp`), but any raster-shaped
    // value must still produce a visible base layer, never an empty style.
    const style = buildMapStyle({ ...base, mode: 'raster', tileFormat: 'webp' });
    expect(style.layers.length).toBeGreaterThan(0);
  });
});

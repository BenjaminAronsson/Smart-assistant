import type { StyleSpecification } from 'maplibre-gl';
import type { MapTileFormatDto } from '../../../generated/api-types';

/**
 * MapLibre style construction (F3b.5, docs/12 §3, ADR-013). Pure — this file
 * only imports MapLibre's **types** (`import type`, erased at build time), so
 * it carries no runtime dependency on the GL bundle and can be unit-tested
 * without a WebGL context.
 */

export type MapTileMode = 'local' | 'raster';

export interface MapStyleInput {
  mode: MapTileMode;
  /** Absolute `{z}/{x}/{y}` tile URL template, ready for a MapLibre source. */
  tileUrlTemplate: string;
  tileFormat: MapTileFormatDto;
  minZoom: number;
  maxZoom: number;
}

const TILE_SOURCE_ID = 'jarvis-map-tiles';

/**
 * Builds the map's base style. Two shapes, chosen by `tileFormat`:
 *
 * - **Raster** (`png`/`jpeg`/`webp`/`avif`) — a single raster source/layer.
 *   The same shape serves *both* a locally served raster PMTiles archive and
 *   the online OSM raster fallback (docs/12 §3): a raster tile is a raster
 *   tile regardless of which server it came from, so `mode` only affects
 *   attribution/zoom bounds upstream of this function, not the style shape.
 * - **Vector** (`mvt`) — a compact, geometry-only style tuned to the
 *   Protomaps Basemap schema, the documented default extract (docs/09 §1,
 *   docs/08 §6: "downloaded regional extract"). Deliberately **no text
 *   labels**: label rendering needs offline glyph/sprite serving, which is
 *   out of scope for this slice (flagged as a follow-up, not a silent gap —
 *   see the F3b.5 commit message). A schema this does not recognize still
 *   renders `background` + whatever named layers happen to match; it never
 *   throws or shows nothing, because every layer here is additive over a
 *   solid background fill.
 */
export function buildMapStyle(input: MapStyleInput): StyleSpecification {
  return input.tileFormat === 'mvt' ? protomapsStyle(input) : rasterStyle(input);
}

function rasterStyle(input: MapStyleInput): StyleSpecification {
  return {
    version: 8,
    sources: {
      [TILE_SOURCE_ID]: {
        type: 'raster',
        tiles: [input.tileUrlTemplate],
        tileSize: 256,
        minzoom: input.minZoom,
        maxzoom: input.maxZoom,
      },
    },
    layers: [{ id: 'jarvis-raster', type: 'raster', source: TILE_SOURCE_ID }],
  };
}

/** Protomaps Basemap schema layer names (stable, versioned upstream). */
function protomapsStyle(input: MapStyleInput): StyleSpecification {
  return {
    version: 8,
    sources: {
      [TILE_SOURCE_ID]: {
        type: 'vector',
        tiles: [input.tileUrlTemplate],
        minzoom: input.minZoom,
        maxzoom: input.maxZoom,
      },
    },
    layers: [
      { id: 'background', type: 'background', paint: { 'background-color': '#eef1f4' } },
      {
        id: 'earth',
        type: 'fill',
        source: TILE_SOURCE_ID,
        'source-layer': 'earth',
        paint: { 'fill-color': '#f4f2ec' },
      },
      {
        id: 'landuse',
        type: 'fill',
        source: TILE_SOURCE_ID,
        'source-layer': 'landuse',
        paint: { 'fill-color': '#e2ead6', 'fill-opacity': 0.6 },
      },
      {
        id: 'water',
        type: 'fill',
        source: TILE_SOURCE_ID,
        'source-layer': 'water',
        paint: { 'fill-color': '#a9cbe0' },
      },
      {
        id: 'buildings',
        type: 'fill',
        source: TILE_SOURCE_ID,
        'source-layer': 'buildings',
        paint: { 'fill-color': '#d9d2c4' },
      },
      {
        id: 'roads',
        type: 'line',
        source: TILE_SOURCE_ID,
        'source-layer': 'roads',
        layout: { 'line-cap': 'round', 'line-join': 'round' },
        paint: {
          'line-color': '#ffffff',
          'line-width': ['interpolate', ['linear'], ['zoom'], 8, 0.5, 16, 3],
        },
      },
      {
        id: 'boundaries',
        type: 'line',
        source: TILE_SOURCE_ID,
        'source-layer': 'boundaries',
        paint: { 'line-color': '#b3aa98', 'line-width': 1, 'line-dasharray': [2, 2] },
      },
    ],
  };
}

import {
  ChangeDetectionStrategy,
  Component,
  ElementRef,
  OnDestroy,
  effect,
  input,
  viewChild,
} from '@angular/core';
import type { Map as MapLibreMap, Marker as MapLibreMarker } from 'maplibre-gl';
import type { MapPointDto, MapTileFormatDto } from '../../../generated/api-types';
import { computeFitBounds } from './map-geo';
import { type MapTileMode, buildMapStyle } from './map-style';

/**
 * The GL rendering surface (F3b.5, docs/12 §3, ADR-013). Split out from
 * `MapCard` for exactly one reason: `maplibre-gl` is a large dependency (the
 * `low-power` skill's "can it be lazy?" rule), and this is the only file in
 * the map card that imports it at the top level. Because this module is only
 * ever reached through `MapCard`'s `@defer` block (see `map-card.html`), the
 * Angular build code-splits `maplibre-gl` into its own chunk — it is never
 * part of the HUD's initial bundle, only fetched once a query actually
 * produces a map card that needs local or raster tiles (never for the
 * coordinates-only degrade, which renders with no GL dependency at all).
 *
 * Deliberately thin: style/marker/bounds *decisions* live in the pure,
 * unit-tested `map-geo.ts` / `map-style.ts`; this component's job is only to
 * hand their output to the real MapLibre `Map` and clean it up on destroy.
 */
@Component({
  selector: 'app-map-gl-view',
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './map-gl-view.html',
  styleUrl: './map-gl-view.scss',
})
export class MapGlView implements OnDestroy {
  readonly mode = input.required<MapTileMode>();
  readonly tileUrlTemplate = input.required<string>();
  readonly tileFormat = input.required<MapTileFormatDto>();
  readonly minZoom = input(0);
  readonly maxZoom = input(19);
  /** Plain text, always rendered, never collapsible (docs/12 §3: OSM
   * attribution is never hidden). */
  readonly attribution = input.required<string>();
  readonly destination = input.required<MapPointDto>();
  readonly currentLocation = input<MapPointDto | null>(null);
  readonly route = input<MapPointDto[]>([]);

  private readonly container = viewChild.required<ElementRef<HTMLDivElement>>('mapContainer');

  private map: MapLibreMap | null = null;
  private markers: MapLibreMarker[] = [];
  private destroyed = false;

  constructor() {
    // Runs once the container is in the DOM; re-running on later input
    // changes would only fight the map's own pan/zoom state, so this effect
    // intentionally reads its inputs exactly once via `untracked`-by-value at
    // construction (the destination/route of a given card instance never
    // change after creation — a new query produces a new card, and thus a new
    // component instance, per `card-id.ts`).
    effect(() => {
      const el = this.container().nativeElement;
      if (this.map) return;
      void this.mount(el);
    });
  }

  ngOnDestroy(): void {
    this.destroyed = true;
    for (const marker of this.markers) marker.remove();
    this.markers = [];
    this.map?.remove();
    this.map = null;
  }

  /**
   * Never throws. A WebGL context can fail to acquire (no GPU, driver
   * unavailable, or the component was destroyed mid-load) — that degrades to
   * an empty container rather than an unhandled rejection, the same
   * "never crash, degrade visibly instead" rule the artifact renderers
   * follow for malformed data.
   */
  private async mount(container: HTMLDivElement): Promise<void> {
    try {
      await this.mountUnsafe(container);
    } catch (error) {
      console.error('map-gl-view: failed to initialize MapLibre', error);
    }
  }

  private async mountUnsafe(container: HTMLDivElement): Promise<void> {
    const maplibregl = await import('maplibre-gl');
    // The component may have been destroyed while the chunk was loading.
    if (this.map || this.destroyed) return;

    const destination = this.destination();
    const style = buildMapStyle({
      mode: this.mode(),
      tileUrlTemplate: this.tileUrlTemplate(),
      tileFormat: this.tileFormat(),
      minZoom: this.minZoom(),
      maxZoom: this.maxZoom(),
    });

    const map = new maplibregl.Map({
      container,
      style,
      center: [destination.lon, destination.lat],
      zoom: Math.min(this.maxZoom(), 14),
      attributionControl: false,
    });
    this.map = map;

    // No `maplibregl.AttributionControl` here (S4): MapLibre renders a
    // control's `customAttribution` by assigning it to
    // `_innerContainer.innerHTML` internally (sanitized, but the library's
    // own comment admits that "might not be enough to prevent all XSS
    // attacks") — a markup sink, when `attribution` is a server-supplied
    // string the contract (`MapCoverageResponse.attribution`) promises is
    // plain text. The attribution is instead rendered as a plain Angular
    // text interpolation in `map-gl-view.html`, permanently visible
    // (docs/12 §3) without ever reaching a markup sink.
    map.addControl(new maplibregl.NavigationControl({ showCompass: false }), 'top-right');

    const currentLocation = this.currentLocation();
    const route = this.route();

    this.markers.push(
      new maplibregl.Marker({ color: getComputedColor('--c-card-accent', '#4a7fd4') })
        .setLngLat([destination.lon, destination.lat])
        .addTo(map),
    );
    if (currentLocation) {
      this.markers.push(
        new maplibregl.Marker({ color: getComputedColor('--ink-dim', '#464d57') })
          .setLngLat([currentLocation.lon, currentLocation.lat])
          .addTo(map),
      );
    }

    map.on('load', () => {
      if (route.length >= 2) {
        map.addSource('jarvis-route', {
          type: 'geojson',
          data: {
            type: 'Feature',
            properties: {},
            geometry: { type: 'LineString', coordinates: route.map((p) => [p.lon, p.lat]) },
          },
        });
        map.addLayer({
          id: 'jarvis-route-line',
          type: 'line',
          source: 'jarvis-route',
          layout: { 'line-cap': 'round', 'line-join': 'round' },
          paint: { 'line-color': getComputedColor('--c-card-accent', '#4a7fd4'), 'line-width': 3 },
        });
      }

      const framePoints = [destination, ...(currentLocation ? [currentLocation] : []), ...route];
      const bounds = computeFitBounds(framePoints);
      if (bounds && framePoints.length > 1) {
        map.fitBounds(bounds, { padding: 48, maxZoom: this.maxZoom(), duration: 0 });
      }
    });
  }
}

/** Reads a `--token` from the document root so marker colors stay in the same
 * palette as the rest of the card grammar (`styles.scss`) instead of a
 * second hardcoded hex living in this file. Falls back to the token's known
 * default when read outside a browser (e.g. a non-DOM test runner). */
function getComputedColor(token: string, fallback: string): string {
  if (typeof document === 'undefined' || typeof getComputedStyle === 'undefined') return fallback;
  const value = getComputedStyle(document.documentElement).getPropertyValue(token).trim();
  return value.length > 0 ? value : fallback;
}

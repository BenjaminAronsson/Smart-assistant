import {
  ChangeDetectionStrategy,
  Component,
  DestroyRef,
  computed,
  inject,
  input,
  signal,
} from '@angular/core';
import type { HudCardDto, MapCoverageResponse, MapTileFormatDto } from '../../../generated/api-types';
import { ApiService } from '../../api.service';
import {
  compassLabel,
  decideMapRenderMode,
  formatCoordinate,
  initialBearingDeg,
  type MapCoverageResult,
  type MapRenderMode,
} from './map-geo';
import { MapGlView } from './map-gl-view';
import type { MapTileMode } from './map-style';

/** Wire shape of this card's own variant, narrowed once by `hud-card`. */
export type MapCardData = Extract<HudCardDto, { type: 'card.map' }>;

/** The online fallback (docs/12 §3): public OSM raster tiles, keyless and
 * needing no jarvisd configuration — this is the "network up" half of the
 * coverage fallback, independent of whatever local archive is (or isn't)
 * configured. */
const OSM_TILE_URL_TEMPLATE = 'https://tile.openstreetmap.org/{z}/{x}/{y}.png';
const OSM_ATTRIBUTION = '© OpenStreetMap contributors';
const OSM_MIN_ZOOM = 0;
const OSM_MAX_ZOOM = 19;

interface TileConfig {
  mode: MapTileMode;
  tileUrlTemplate: string;
  tileFormat: MapTileFormatDto;
  minZoom: number;
  maxZoom: number;
  attribution: string;
}

type CoverageState =
  | { status: 'loading' }
  | { status: 'available'; data: MapCoverageResponse }
  /** `GET /api/v1/map/coverage` 404'd — no archive configured (docs/09 §1). */
  | { status: 'unconfigured' }
  /** The request itself failed — treated the same as "not covered" by the
   * fallback decision, never assumed to be in-region (docs/12 §3). */
  | { status: 'error' };

/**
 * Map card (F3b.5, docs/12 §3, ADR-013): destination pin, current-location
 * dot, route polyline, and the coverage fallback that keeps this card from
 * ever going blank or showing the wrong region. Three faces, chosen by
 * {@link decideMapRenderMode}:
 *
 * 1. **Local** — the destination is inside the region jarvisd serves from its
 *    PMTiles archive (`GET /api/v1/map/coverage`); MapLibre renders it with
 *    no tile request ever leaving the machine.
 * 2. **Raster** — outside that region (or no archive configured) but the
 *    network is up: online OSM raster tiles, still through the same
 *    `MapGlView`, same attribution rule.
 * 3. **Coords** — outside coverage and offline: no interactive map at all,
 *    just coordinates, distance and (when a current-location point is
 *    carried) a compass bearing.
 *
 * This is the one HUD card that reaches for `ApiService` itself rather than
 * receiving all its data as inputs (contrast `TimerCard`/`MediaBar`, which
 * are pure and let their host own every request). The deviation is
 * deliberate: the coverage lookup is a read with no optimistic-UI or command
 * coordination need — nothing here posts, nothing needs a pending/error
 * state threaded from a host — so routing it through `HudStateService` would
 * add a layer for no behavioral gain.
 */
@Component({
  selector: 'app-map-card',
  imports: [MapGlView],
  changeDetection: ChangeDetectionStrategy.OnPush,
  templateUrl: './map-card.html',
  styleUrl: './map-card.scss',
  host: {
    // Debugging/testing hook, same convention as `HudCard`'s `data-card-type`.
    '[attr.data-map-mode]': 'mode()',
  },
})
export class MapCard {
  readonly card = input.required<MapCardData>();

  private readonly api = inject(ApiService);
  private readonly destroyRef = inject(DestroyRef);

  private readonly coverageState = signal<CoverageState>({ status: 'loading' });
  private readonly online = signal(typeof navigator === 'undefined' ? true : navigator.onLine);
  protected readonly expanded = signal(false);

  protected readonly mode = computed<MapRenderMode | 'loading'>(() => {
    const state = this.coverageState();
    if (state.status === 'loading') return 'loading';
    const result: MapCoverageResult =
      state.status === 'available'
        ? { kind: 'available', bounds: state.data.bounds }
        : state.status === 'unconfigured'
          ? { kind: 'unconfigured' }
          : { kind: 'unknown' };
    return decideMapRenderMode(result, this.card().destination, this.online());
  });

  /** The tile config to hand `MapGlView`, for whichever of the two GL-backed
   * modes is active. `null` for `loading`/`coords` — nothing to render. */
  protected readonly activeTileConfig = computed<TileConfig | null>(() => {
    const mode = this.mode();
    if (mode === 'local') {
      const state = this.coverageState();
      if (state.status !== 'available') return null;
      const data = state.data;
      return {
        mode: 'local',
        tileUrlTemplate: data.tileUrlTemplate,
        tileFormat: data.tileFormat,
        minZoom: data.minZoom,
        maxZoom: data.maxZoom,
        attribution: data.attribution,
      };
    }
    if (mode === 'raster') {
      return {
        mode: 'raster',
        tileUrlTemplate: OSM_TILE_URL_TEMPLATE,
        tileFormat: 'png',
        minZoom: OSM_MIN_ZOOM,
        maxZoom: OSM_MAX_ZOOM,
        attribution: OSM_ATTRIBUTION,
      };
    }
    return null;
  });

  /** Coordinates-only degrade text (docs/12 §3: "lat/long, distance and
   * bearing from home, no interactive map"). */
  protected readonly coordsText = computed(() => formatCoordinate(this.card().destination));

  protected readonly bearingText = computed<string | null>(() => {
    const from = this.card().currentLocation;
    if (!from) return null;
    const bearing = initialBearingDeg(from, this.card().destination);
    return `${Math.round(bearing)}° ${compassLabel(bearing)} from current location`;
  });

  constructor() {
    this.loadCoverage();

    const onOnline = () => this.online.set(true);
    const onOffline = () => this.online.set(false);
    const onKeydown = (event: KeyboardEvent) => {
      if (event.key === 'Escape' && this.expanded()) {
        this.expanded.set(false);
      }
    };
    if (typeof window !== 'undefined') {
      window.addEventListener('online', onOnline);
      window.addEventListener('offline', onOffline);
      window.addEventListener('keydown', onKeydown);
    }
    this.destroyRef.onDestroy(() => {
      if (typeof window === 'undefined') return;
      window.removeEventListener('online', onOnline);
      window.removeEventListener('offline', onOffline);
      window.removeEventListener('keydown', onKeydown);
    });
  }

  protected setExpanded(value: boolean): void {
    this.expanded.set(value);
  }

  private async loadCoverage(): Promise<void> {
    try {
      const coverage = await this.api.getMapCoverage();
      this.coverageState.set(
        coverage ? { status: 'available', data: coverage } : { status: 'unconfigured' },
      );
    } catch {
      this.coverageState.set({ status: 'error' });
    }
  }
}

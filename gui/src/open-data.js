/**
 * Open Data Sources — zero-config replacements for Cesium Ion.
 *
 * Provides global terrain, geocoding, OSM buildings, and imagery
 * without any API keys or Ion tokens.
 */
import * as Cesium from 'cesium';

// ─── Terrain ─────────────────────────────────────────────────────────────────

/**
 * Create a terrain provider from open data.
 *
 * Priority:
 *  1. Local TileTopia server quantized-mesh terrain (if running)
 *  2. Ellipsoid (flat) fallback — always available
 */
export async function createOpenTerrain(apiBase = '/api/v1') {
  try {
    const res = await fetch(`${apiBase}/terrain/layer.json`, { signal: AbortSignal.timeout(2000) });
    if (res.ok) {
      return await Cesium.CesiumTerrainProvider.fromUrl(`${apiBase}/terrain`);
    }
  } catch {
    // Server not running — fall back
  }
  return new Cesium.EllipsoidTerrainProvider();
}

// ─── Imagery Layers ──────────────────────────────────────────────────────────

/** OSM raster tiles — the default, no key needed. */
export function osmImageryProvider() {
  return new Cesium.OpenStreetMapImageryProvider({
    url: 'https://tile.openstreetmap.org/',
  });
}

/** Stadia Stamen Toner — good for data overlays. */
export function stamenTonerProvider() {
  return new Cesium.UrlTemplateImageryProvider({
    url: 'https://tiles.stadiamaps.com/tiles/stamen_toner/{z}/{x}/{y}.png',
    credit: '&copy; Stamen Design &copy; OpenStreetMap contributors',
    maximumLevel: 18,
  });
}

/** ESRI World Imagery (free, attribution required). */
export function esriWorldImageryProvider() {
  return new Cesium.ArcGisMapServerImageryProvider({
    url: 'https://services.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer',
  });
}

// ─── Geocoding (Nominatim) ──────────────────────────────────────────────────

/**
 * Nominatim-backed geocoder for the CesiumJS search bar.
 *
 * Respects Nominatim usage policy (1 req/s, User-Agent).
 */
export class NominatimGeocoder {
  async geocode(input) {
    const encoded = encodeURIComponent(input);
    const url = `https://nominatim.openstreetmap.org/search?format=json&q=${encoded}&limit=5`;
    const res = await fetch(url, {
      headers: { 'User-Agent': 'TileTopia-Viewer/0.3.0' },
    });
    if (!res.ok) return [];
    const results = await res.json();
    return results.map((r) => ({
      displayName: r.display_name,
      destination: Cesium.Cartesian3.fromDegrees(
        parseFloat(r.lon),
        parseFloat(r.lat),
        1000,
      ),
    }));
  }
}

// ─── OSM Buildings (Overpass API, client-side) ──────────────────────────────

/**
 * Fetch OSM buildings from Overpass API for the current view and create
 * extruded Cesium entities.  Works entirely client-side — no server needed.
 *
 * @param {Cesium.Viewer} viewer
 * @param {object} [opts]
 * @param {number} [opts.maxArea=0.5] Max bbox area in degrees² to avoid huge queries
 */
export async function loadOsmBuildings(viewer, opts = {}) {
  const maxArea = opts.maxArea ?? 0.5;
  let rect = viewer.camera.computeViewRectangle();

  // computeViewRectangle can return undefined in 3D mode — fall back to a bbox from camera position
  if (!rect) {
    const carto = viewer.camera.positionCartographic;
    if (!carto) return [];
    const span = 0.005; // ~500m
    rect = new Cesium.Rectangle(
      carto.longitude - span,
      carto.latitude - span,
      carto.longitude + span,
      carto.latitude + span,
    );
  }

  const south = Cesium.Math.toDegrees(rect.south);
  const west = Cesium.Math.toDegrees(rect.west);
  const north = Cesium.Math.toDegrees(rect.north);
  const east = Cesium.Math.toDegrees(rect.east);

  if ((north - south) * (east - west) > maxArea) {
    console.warn('View too wide for OSM building query — zoom in');
    return [];
  }

  const bbox = `${south},${west},${north},${east}`;
  const query = `[out:json][timeout:15];way["building"](${bbox});out body;>;out skel qt;`;
  const res = await fetch('https://overpass-api.de/api/interpreter', {
    method: 'POST',
    body: `data=${encodeURIComponent(query)}`,
    headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
  });
  if (!res.ok) return [];

  const data = await res.json();
  const nodes = new Map();
  const ways = [];
  for (const el of data.elements) {
    if (el.type === 'node') nodes.set(el.id, el);
    else if (el.type === 'way') ways.push(el);
  }
  console.log(`OSM: fetched ${nodes.size} nodes, ${ways.length} ways for bbox ${bbox}`);

  const entities = [];
  for (const way of ways) {
    const coords = way.nodes
      .map((id) => nodes.get(id))
      .filter(Boolean)
      .flatMap((n) => [n.lon, n.lat]);
    if (coords.length < 6) continue;

    const levels = parseInt(way.tags?.['building:levels'] ?? '3', 10);
    const height = parseFloat(way.tags?.['height'] ?? String(levels * 3.2));

    entities.push(
      viewer.entities.add({
        polygon: {
          hierarchy: Cesium.Cartesian3.fromDegreesArray(coords),
          height: 0,
          extrudedHeight: height,
          material: Cesium.Color.fromCssColorString(way.tags?.['building:colour'] ?? '#c8b896').withAlpha(0.85),
          outline: true,
          outlineColor: Cesium.Color.BLACK.withAlpha(0.3),
        },
        properties: way.tags,
      }),
    );
  }
  return entities;
}

// ─── Zero-Config Setup ──────────────────────────────────────────────────────

/**
 * Apply all open data sources to a viewer.  Call once after viewer creation.
 *
 * ```js
 * import { applyOpenData } from './open-data.js';
 * const viewer = new Cesium.Viewer('cesium-container', { ... });
 * await applyOpenData(viewer);
 * ```
 */
export async function applyOpenData(viewer, opts = {}) {
  const apiBase = opts.apiBase ?? '/api/v1';

  // Terrain
  viewer.terrainProvider = await createOpenTerrain(apiBase);

  // Geocoder — replace the default (which requires Ion)
  // CesiumJS doesn't expose a setter for geocoder services after init,
  // so we inject our provider into the geocoder viewModel.
  if (viewer.geocoder) {
    const vm = viewer.geocoder.viewModel;
    vm._geocoderServices = [new NominatimGeocoder()];
  }

  // Optional: Google Photorealistic 3D Tiles (needs API key)
  const googleKey = opts.google3dTilesKey ?? (typeof import.meta !== 'undefined' && import.meta.env?.VITE_GOOGLE_3D_TILES_KEY);
  if (googleKey) {
    try {
      const tileset = await Cesium.Cesium3DTileset.fromUrl(
        `https://tile.googleapis.com/v1/3dtiles/root.json?key=${googleKey}`,
      );
      viewer.scene.primitives.add(tileset);
    } catch (e) {
      console.warn('Failed to load Google 3D Tiles:', e);
    }
  }
}

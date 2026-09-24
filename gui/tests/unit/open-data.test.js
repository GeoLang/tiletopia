import { describe, it, expect, vi, beforeEach } from 'vitest';
import { GeokodeGeocoder } from '../../src/open-data.js';

// Mock Cesium globals needed by open-data.js
vi.mock('cesium', () => {
  const toDegrees = (rad) => (rad * 180) / Math.PI;
  const toRadians = (deg) => (deg * Math.PI) / 180;
  return {
    default: {},
    Math: { toDegrees, toRadians },
    Cartesian3: {
      fromDegrees: (lon, lat, h) => ({ x: lon, y: lat, z: h }),
      fromDegreesArray: (arr) => {
        const out = [];
        for (let i = 0; i < arr.length; i += 2) {
          out.push({ x: arr[i], y: arr[i + 1], z: 0 });
        }
        return out;
      },
    },
    Color: {
      fromCssColorString: (c) => ({
        withAlpha: (a) => ({ color: c, alpha: a }),
      }),
      BLACK: { withAlpha: (a) => ({ color: '#000', alpha: a }) },
    },
    Rectangle: class {
      constructor(w, s, e, n) {
        this.west = w;
        this.south = s;
        this.east = e;
        this.north = n;
      }

      static fromDegrees(w, s, e, n) {
        return { west: w, south: s, east: e, north: n };
      }
    },
    OpenStreetMapImageryProvider: class {
      constructor(opts) { this.url = opts.url; }
    },
    UrlTemplateImageryProvider: class {
      constructor(opts) { this.url = opts.url; }
    },
    ArcGisMapServerImageryProvider: class {
      constructor(opts) { this.url = opts.url; }
    },
    EllipsoidTerrainProvider: class {},
    CesiumTerrainProvider: {
      fromUrl: vi.fn().mockResolvedValue({}),
    },
    HeightReference: { RELATIVE_TO_GROUND: 1 },
  };
});

const VIEWER_ORIGIN = 'http://viewer.test';

function geokodeResult(fields) {
  return {
    name: null,
    display_name: '',
    address: {},
    country_code: null,
    lat: 0,
    lon: 0,
    bbox: null,
    kind: 'place',
    osm_type: null,
    osm_id: null,
    osm_key: null,
    osm_value: null,
    admin_level: null,
    population: null,
    confidence: 1,
    match_type: 'exact',
    ...fields,
  };
}

function answerWith(results) {
  global.fetch = vi.fn().mockResolvedValue({
    ok: true,
    json: () => Promise.resolve({ results }),
  });
}

describe('GeokodeGeocoder', () => {
  const geocoder = new GeokodeGeocoder();

  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it('asks the same-origin geokode forward path with a limit', async () => {
    answerWith([]);
    await geocoder.geocode('New York & Co');
    expect(global.fetch).toHaveBeenCalledTimes(1);
    const url = new URL(global.fetch.mock.calls[0][0], VIEWER_ORIGIN);
    expect(url.origin).toBe(VIEWER_ORIGIN);
    expect(url.pathname).toBe('/api/geocode/forward');
    expect(url.searchParams.get('q')).toBe('New York & Co');
    expect(url.searchParams.get('limit')).toBe('5');
  });

  it('maps display_name and bbox to a rectangle destination', async () => {
    answerWith([
      geokodeResult({
        display_name: 'Zurich, Switzerland',
        lat: 47.37,
        lon: 8.54,
        bbox: [8.44, 47.32, 8.63, 47.43],
      }),
    ]);
    const results = await geocoder.geocode('Zurich');
    expect(results).toEqual([
      {
        displayName: 'Zurich, Switzerland',
        destination: { west: 8.44, south: 47.32, east: 8.63, north: 47.43 },
      },
    ]);
  });

  it('falls back to lat and lon when a result has no bbox', async () => {
    answerWith([geokodeResult({ display_name: 'Bahnhofstrasse 1', lat: 47.37, lon: 8.54 })]);
    const results = await geocoder.geocode('Bahnhofstrasse 1');
    expect(results[0].destination).toEqual({ x: 8.54, y: 47.37, z: 1000 });
  });

  it('returns an empty list on a miss', async () => {
    answerWith([]);
    expect(await geocoder.geocode('nowhere')).toEqual([]);
  });

  it('returns an empty list on an HTTP error', async () => {
    global.fetch = vi.fn().mockResolvedValue({
      ok: false,
      status: 400,
      json: () => Promise.resolve({ error: 'q must be 1 to 256 characters' }),
    });
    expect(await geocoder.geocode('x'.repeat(300))).toEqual([]);
    expect(global.fetch).toHaveBeenCalledTimes(1);
  });

  it('returns an empty list on a network error', async () => {
    global.fetch = vi.fn().mockRejectedValue(new Error('network'));
    expect(await geocoder.geocode('Zurich')).toEqual([]);
    expect(global.fetch).toHaveBeenCalledTimes(1);
  });
});

describe('osmImageryProvider', () => {
  it('creates OSM provider', async () => {
    const { osmImageryProvider } = await import('../../src/open-data.js');
    const provider = osmImageryProvider();
    expect(provider.url).toBe('https://tile.openstreetmap.org/');
  });
});

describe('createOpenTerrain', () => {
  it('falls back to ellipsoid when server is unavailable', async () => {
    global.fetch = vi.fn().mockRejectedValue(new Error('ECONNREFUSED'));
    const { createOpenTerrain } = await import('../../src/open-data.js');
    const provider = await createOpenTerrain('/api/v1');
    expect(provider.constructor.name).toBe('EllipsoidTerrainProvider');
  });
});

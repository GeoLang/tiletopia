import { describe, it, expect, vi, beforeEach } from 'vitest';
import { NominatimGeocoder } from '../../src/open-data.js';

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

describe('NominatimGeocoder', () => {
  const geocoder = new NominatimGeocoder();

  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it('returns empty array on network error', async () => {
    global.fetch = vi.fn().mockRejectedValue(new Error('network'));
    // geocode should not throw
    await expect(geocoder.geocode('test')).rejects.toThrow();
  });

  it('returns empty array on non-ok response', async () => {
    global.fetch = vi.fn().mockResolvedValue({ ok: false });
    const results = await geocoder.geocode('test');
    expect(results).toEqual([]);
  });

  it('parses Nominatim results', async () => {
    global.fetch = vi.fn().mockResolvedValue({
      ok: true,
      json: () =>
        Promise.resolve([
          { display_name: 'Paris, France', lon: '2.3522', lat: '48.8566' },
        ]),
    });
    const results = await geocoder.geocode('Paris');
    expect(results).toHaveLength(1);
    expect(results[0].displayName).toBe('Paris, France');
    expect(results[0].destination).toEqual({ x: 2.3522, y: 48.8566, z: 1000 });
  });

  it('encodes special characters in search query', async () => {
    global.fetch = vi.fn().mockResolvedValue({ ok: true, json: () => Promise.resolve([]) });
    await geocoder.geocode('New York & Co');
    const url = global.fetch.mock.calls[0][0];
    expect(url).toContain('New%20York%20%26%20Co');
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

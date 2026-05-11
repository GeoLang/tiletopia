import { describe, it, expect } from 'vitest';

describe('measurement calculations', () => {
  it('computes haversine distance correctly', () => {
    // Simple distance check: NYC to London ~5570 km
    const R = 6371e3; // metres
    const toRad = (d) => (d * Math.PI) / 180;
    const lat1 = toRad(40.7128), lat2 = toRad(51.5074);
    const dLat = lat2 - lat1;
    const dLon = toRad(-0.1278 - -74.006);
    const a = Math.sin(dLat / 2) ** 2 + Math.cos(lat1) * Math.cos(lat2) * Math.sin(dLon / 2) ** 2;
    const dist = 2 * R * Math.asin(Math.sqrt(a));
    expect(dist / 1000).toBeCloseTo(5570, -1); // within 10 km
  });

  it('computes polygon area using shoelace formula', () => {
    // Unit square
    const coords = [[0, 0], [1, 0], [1, 1], [0, 1]];
    let area = 0;
    const n = coords.length;
    for (let i = 0; i < n; i++) {
      const j = (i + 1) % n;
      area += coords[i][0] * coords[j][1];
      area -= coords[j][0] * coords[i][1];
    }
    area = Math.abs(area) / 2;
    expect(area).toBe(1);
  });
});

describe('bbox calculations', () => {
  it('rejects area exceeding maxArea threshold', () => {
    const maxArea = 0.5;
    const south = 37.0, north = 38.5, west = -123.0, east = -121.0;
    const area = (north - south) * (east - west);
    expect(area).toBe(3.0);
    expect(area > maxArea).toBe(true);
  });

  it('accepts small neighborhood bbox', () => {
    const maxArea = 0.5;
    const south = 37.77, north = 37.78, west = -122.42, east = -122.41;
    const area = (north - south) * (east - west);
    expect(area).toBeLessThan(maxArea);
  });
});

/**
 * Classification Visualization — color point cloud tiles by ASPRS class.
 *
 * Applies per-point styling to 3D Tiles based on classification attribute.
 */
import * as Cesium from 'cesium';

/** ASPRS class colors (matching the server-side palette). */
const CLASS_COLORS = {
  0: [200, 200, 200], // Unclassified
  2: [139, 90, 43],   // Ground
  3: [144, 238, 144], // Low Vegetation
  4: [34, 139, 34],   // Medium Vegetation
  5: [0, 100, 0],     // High Vegetation
  6: [255, 69, 0],    // Building
  7: [255, 0, 255],   // Noise
  9: [0, 100, 255],   // Water
  11: [64, 64, 64],   // Road
  14: [255, 255, 0],  // Power Line
  15: [255, 165, 0],  // Transmission Tower
  17: [160, 82, 45],  // Bridge
  19: [0, 255, 255],  // Pole
  64: [255, 20, 147], // Vehicle
};

const CLASS_NAMES = {
  0: 'Unclassified', 2: 'Ground', 3: 'Low Vegetation', 4: 'Medium Vegetation',
  5: 'High Vegetation', 6: 'Building', 7: 'Noise', 9: 'Water', 11: 'Road',
  14: 'Power Line', 15: 'Transmission Tower', 17: 'Bridge', 19: 'Pole', 64: 'Vehicle',
};

/**
 * Apply classification coloring to a 3D Tileset.
 *
 * @param {Cesium.Cesium3DTileset} tileset
 * @param {string} [attribute='Classification'] — the point attribute name
 */
export function applyClassificationStyle(tileset, attribute = 'Classification') {
  const conditions = Object.entries(CLASS_COLORS).map(([code, [r, g, b]]) =>
    [`\${${attribute}} === ${code}`, `color("rgb(${r},${g},${b})")`]
  );
  conditions.push(['true', 'color("rgb(200,200,200)")']); // fallback

  tileset.style = new Cesium.Cesium3DTileStyle({
    color: { conditions },
    pointSize: 3,
  });
}

/**
 * Apply a single-class highlight (show only one class, dim others).
 *
 * @param {Cesium.Cesium3DTileset} tileset
 * @param {number} classCode - ASPRS class code to highlight
 */
export function highlightClass(tileset, classCode, attribute = 'Classification') {
  const [r, g, b] = CLASS_COLORS[classCode] ?? [200, 200, 200];
  tileset.style = new Cesium.Cesium3DTileStyle({
    color: {
      conditions: [
        [`\${${attribute}} === ${classCode}`, `color("rgb(${r},${g},${b})")`],
        ['true', 'color("rgb(80,80,80)", 0.2)'],
      ],
    },
    pointSize: {
      conditions: [
        [`\${${attribute}} === ${classCode}`, '5'],
        ['true', '1'],
      ],
    },
  });
}

/**
 * Remove classification styling (restore default).
 *
 * @param {Cesium.Cesium3DTileset} tileset
 */
export function clearClassificationStyle(tileset) {
  tileset.style = undefined;
}

/**
 * Get classification statistics from tileset metadata.
 *
 * @param {Cesium.Cesium3DTileset} tileset
 * @returns {Object} class code → count mapping
 */
export function getClassStats(tileset) {
  // This reads from 3D Tiles metadata if available
  const stats = {};
  const metadata = tileset.metadata;
  if (metadata) {
    try {
      const classStats = metadata.getProperty('classificationStatistics');
      if (classStats) return classStats;
    } catch {
      // No metadata available
    }
  }
  return stats;
}

/**
 * Request server-side ML classification for a tileset.
 *
 * @param {string} assetId
 * @param {string} [modelId] — specific model UUID, or omit for default
 * @param {string} [apiBase='/api/v1']
 * @returns {Promise<Object>} classification result
 */
export async function requestClassification(assetId, modelId, apiBase = '/api/v1') {
  const body = { asset_id: assetId };
  if (modelId) body.model_id = modelId;

  const res = await fetch(`${apiBase}/classify`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  return res.json();
}

/**
 * Create a legend element for the classification palette.
 *
 * @returns {HTMLElement}
 */
export function createClassLegend() {
  const legend = document.createElement('div');
  legend.id = 'class-legend';
  legend.style.cssText = `
    position: absolute; bottom: 40px; right: 10px; background: rgba(0,0,0,0.8);
    padding: 10px; border-radius: 6px; color: #fff; font-size: 12px; z-index: 100;
  `;
  legend.innerHTML = '<strong>Classification</strong><br>';
  for (const [code, name] of Object.entries(CLASS_NAMES)) {
    const [r, g, b] = CLASS_COLORS[code] ?? [200, 200, 200];
    legend.innerHTML += `
      <div style="display:flex;align-items:center;gap:6px;margin:2px 0;cursor:pointer"
           data-class="${code}">
        <span style="width:12px;height:12px;background:rgb(${r},${g},${b});display:inline-block;border-radius:2px"></span>
        <span>${name}</span>
      </div>
    `;
  }
  return legend;
}

export { CLASS_COLORS, CLASS_NAMES };

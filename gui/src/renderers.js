/**
 * Multi-renderer abstraction for TileTopia.
 *
 * Supports switching between:
 * - CesiumJS (3D globe, terrain, 3D Tiles)
 * - deck.gl (WebGL2 data visualization, 3D Tiles via loaders.gl)
 * - MapLibre GL JS (vector tiles, 2.5D buildings, terrain)
 */

import { Deck } from '@deck.gl/core';
import { Tile3DLayer } from '@deck.gl/geo-layers';
import { Tiles3DLoader } from '@loaders.gl/3d-tiles';
import maplibregl from 'maplibre-gl';
import 'maplibre-gl/dist/maplibre-gl.css';

const API = '/api/v1';

/** Active renderer state */
let activeRenderer = null;
let cesiumViewer = null; // keep reference from main.js

export function setCesiumViewer(viewer) {
  cesiumViewer = viewer;
}

/**
 * Initialize the renderer selector UI.
 */
export function initRendererSelector() {
  const select = document.getElementById('renderer-choice');
  if (!select) return;

  select.addEventListener('change', (e) => {
    switchRenderer(e.target.value);
  });
}

/**
 * Switch to a different renderer.
 */
export function switchRenderer(renderer) {
  // Clean up previous non-Cesium renderer
  cleanupActiveRenderer();

  const container = document.getElementById('cesium-container');

  switch (renderer) {
    case 'cesium':
      showCesium(container);
      break;
    case 'deckgl':
      hideCesium(container);
      initDeckGL(container);
      break;
    case 'maplibre':
      hideCesium(container);
      initMapLibre(container);
      break;
  }
}

function showCesium(container) {
  // Remove overlay canvas if present
  const overlay = container.querySelector('.renderer-overlay');
  if (overlay) overlay.remove();
  // Show Cesium's own elements
  container.querySelectorAll('.cesium-widget').forEach(el => el.style.display = '');
  if (cesiumViewer) cesiumViewer.resize();
}

function hideCesium(container) {
  container.querySelectorAll('.cesium-widget').forEach(el => el.style.display = 'none');
}

function cleanupActiveRenderer() {
  if (activeRenderer) {
    if (activeRenderer.type === 'deckgl' && activeRenderer.deck) {
      activeRenderer.deck.finalize();
    }
    if (activeRenderer.type === 'maplibre' && activeRenderer.map) {
      activeRenderer.map.remove();
    }
    // Remove overlay element
    const overlay = document.querySelector('.renderer-overlay');
    if (overlay) overlay.remove();
    activeRenderer = null;
  }
}

/**
 * Initialize deck.gl with a dark basemap and 3D Tiles support.
 */
function initDeckGL(container) {
  // Create overlay canvas
  const overlay = document.createElement('div');
  overlay.className = 'renderer-overlay';
  overlay.style.cssText = 'position:absolute;top:0;left:0;width:100%;height:100%;z-index:10;';
  container.appendChild(overlay);

  const deck = new Deck({
    parent: overlay,
    initialViewState: {
      longitude: -122.4,
      latitude: 37.8,
      zoom: 11,
      pitch: 45,
      bearing: 0,
    },
    controller: true,
    layers: [
      new Tile3DLayer({
        id: 'osm-buildings',
        data: 'https://tile.openstreetmap.org/{z}/{x}/{y}.png',
        loader: Tiles3DLoader,
        pointSize: 2,
      }),
    ],
    getTooltip: ({ object }) => object && JSON.stringify(object.properties),
  });

  activeRenderer = { type: 'deckgl', deck };
}

/**
 * Initialize MapLibre GL with vector tiles and 3D terrain.
 */
function initMapLibre(container) {
  const overlay = document.createElement('div');
  overlay.className = 'renderer-overlay';
  overlay.id = 'maplibre-container';
  overlay.style.cssText = 'position:absolute;top:0;left:0;width:100%;height:100%;z-index:10;';
  container.appendChild(overlay);

  const map = new maplibregl.Map({
    container: 'maplibre-container',
    style: {
      version: 8,
      name: 'TileTopia Dark',
      sources: {
        'osm-raster': {
          type: 'raster',
          tiles: ['https://tile.openstreetmap.org/{z}/{x}/{y}.png'],
          tileSize: 256,
          attribution: '&copy; OpenStreetMap contributors',
        },
        'terrain-dem': {
          type: 'raster-dem',
          tiles: [`${window.location.origin}${API}/terrain/{z}/{x}/{y}.terrain`],
          tileSize: 256,
          maxzoom: 14,
        },
      },
      layers: [
        {
          id: 'osm-tiles',
          type: 'raster',
          source: 'osm-raster',
          minzoom: 0,
          maxzoom: 19,
        },
      ],
      terrain: {
        source: 'terrain-dem',
        exaggeration: 1.5,
      },
    },
    center: [-122.4, 37.8],
    zoom: 11,
    pitch: 45,
  });

  map.addControl(new maplibregl.NavigationControl());
  map.addControl(new maplibregl.TerrainControl({ source: 'terrain-dem', exaggeration: 1.5 }));

  activeRenderer = { type: 'maplibre', map };
}

/**
 * Get info about available renderers.
 */
export function getRendererInfo() {
  return [
    {
      id: 'cesium',
      name: 'CesiumJS',
      description: '3D globe with terrain, 3D Tiles, OGC standards',
      features: ['3D Globe', 'Quantized Mesh Terrain', '3D Tiles', 'Time-dynamic'],
    },
    {
      id: 'deckgl',
      name: 'deck.gl',
      description: 'High-performance WebGL2 data visualization',
      features: ['GPU Instancing', '3D Tiles (loaders.gl)', 'Large Point Clouds', 'Custom Shaders'],
    },
    {
      id: 'maplibre',
      name: 'MapLibre GL JS',
      description: 'Open-source vector maps with 3D terrain',
      features: ['Vector Tiles', '3D Terrain', '3D Buildings', 'Custom Styles'],
    },
  ];
}

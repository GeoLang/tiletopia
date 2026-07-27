/**
 * Multi-renderer abstraction for TileTopia.
 *
 * Supports switching between:
 * - CesiumJS (3D globe, terrain, 3D Tiles)
 * - deck.gl (WebGL2 data visualization, 3D Tiles via loaders.gl)
 * - MapLibre GL JS (vector tiles, 2.5D buildings, terrain)
 */

import { Deck } from '@deck.gl/core';
import { Tile3DLayer, TileLayer } from '@deck.gl/geo-layers';
import { BitmapLayer } from '@deck.gl/layers';
import { Tiles3DLoader } from '@loaders.gl/3d-tiles';
import maplibregl from 'maplibre-gl';
import 'maplibre-gl/dist/maplibre-gl.css';
import * as Cesium from 'cesium';

const API = '/api/v1';

/** Active renderer state */
let activeRenderer = null;
let cesiumViewer = null; // keep reference from main.js
let sharedCamera = { longitude: -122.4, latitude: 37.8, zoom: 11, pitch: 45, bearing: 0 };
let deckViewState = null; // module-level to avoid timing issues

/** Extract camera state from the currently active renderer. */
function captureCamera() {
  if (activeRenderer?.type === 'deckgl') {
    const vs = deckViewState;
    if (vs) {
      sharedCamera = { longitude: vs.longitude, latitude: vs.latitude, zoom: vs.zoom, pitch: vs.pitch ?? 45, bearing: vs.bearing ?? 0 };
    }
  } else if (activeRenderer?.type === 'maplibre' && activeRenderer.map) {
    const c = activeRenderer.map.getCenter();
    sharedCamera = { longitude: c.lng, latitude: c.lat, zoom: activeRenderer.map.getZoom(), pitch: activeRenderer.map.getPitch(), bearing: activeRenderer.map.getBearing() };
  } else if (cesiumViewer) {
    const carto = cesiumViewer.camera.positionCartographic;
    if (carto) {
      sharedCamera = {
        longitude: Cesium.Math.toDegrees(carto.longitude),
        latitude: Cesium.Math.toDegrees(carto.latitude),
        zoom: Math.max(0, Math.log2(4e7 / Math.max(carto.height, 1))),
        pitch: Cesium.Math.toDegrees(-cesiumViewer.camera.pitch) || 45,
        bearing: Cesium.Math.toDegrees(cesiumViewer.camera.heading) || 0,
      };
    }
  }
}

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
  // Capture camera from outgoing renderer
  captureCamera();

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
  if (cesiumViewer) {
    cesiumViewer.resize();
    // Restore camera from shared state
    const height = 4e7 / Math.pow(2, sharedCamera.zoom);
    cesiumViewer.camera.flyTo({
      destination: Cesium.Cartesian3.fromDegrees(sharedCamera.longitude, sharedCamera.latitude, height),
      orientation: {
        heading: Cesium.Math.toRadians(sharedCamera.bearing),
        pitch: Cesium.Math.toRadians(-sharedCamera.pitch),
        roll: 0,
      },
      duration: 0,
    });
  }
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

  const initState = {
      longitude: sharedCamera.longitude,
      latitude: sharedCamera.latitude,
      zoom: sharedCamera.zoom,
      pitch: sharedCamera.pitch,
      bearing: sharedCamera.bearing,
    };
  deckViewState = initState;

  const deck = new Deck({
    parent: overlay,
    initialViewState: initState,
    controller: true,
    onViewStateChange: ({ viewState }) => {
      deckViewState = viewState;
    },
    layers: [
      new TileLayer({
        id: 'osm-basemap',
        data: 'https://tile.openstreetmap.org/{z}/{x}/{y}.png',
        minZoom: 0,
        maxZoom: 19,
        tileSize: 256,
        renderSubLayers: (props) => {
          const { boundingBox } = props.tile;
          return new BitmapLayer(props, {
            data: null,
            image: props.data,
            bounds: [boundingBox[0][0], boundingBox[0][1], boundingBox[1][0], boundingBox[1][1]],
          });
        },
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
          // terrain-RGB, not the quantized mesh: that one is geographic and
          // binary, and MapLibre reads neither
          tiles: [`${window.location.origin}${API}/terrain/rgb/{z}/{x}/{y}.png`],
          encoding: 'mapbox',
          tileSize: 256,
          maxzoom: 15,
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
    center: [sharedCamera.longitude, sharedCamera.latitude],
    zoom: sharedCamera.zoom,
    pitch: sharedCamera.pitch,
    bearing: sharedCamera.bearing,
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

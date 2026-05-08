import * as Cesium from 'cesium';
import 'cesium/Build/Cesium/Widgets/widgets.css';
import './style.css';

// API base URL (proxied in dev, same-origin in production)
const API = '/api/v1';

// Initialize Cesium viewer
Cesium.Ion.defaultAccessToken = ''; // No Ion needed — self-hosted tiles
const viewer = new Cesium.Viewer('cesium-container', {
  terrain: undefined,
  baseLayerPicker: false,
  geocoder: false,
  animation: false,
  timeline: false,
  homeButton: true,
  sceneModePicker: true,
  navigationHelpButton: false,
  infoBox: true,
  selectionIndicator: true,
});

// Track loaded tilesets
const loadedTilesets = new Map();

// Check server health
async function checkHealth() {
  const dot = document.querySelector('.status-dot');
  const text = document.getElementById('status-text');
  try {
    const res = await fetch(`${API}/health`);
    if (res.ok) {
      dot.classList.add('connected');
      const data = await res.json();
      text.textContent = `Connected (v${data.version})`;
    }
  } catch {
    dot.classList.remove('connected');
    text.textContent = 'Disconnected';
  }
}

// Fetch and display assets
async function loadAssets() {
  const list = document.getElementById('asset-list');
  try {
    const res = await fetch(`${API}/assets`);
    const assets = await res.json();
    list.innerHTML = assets.map(a => `
      <div class="asset-item" data-id="${a.id}">
        <div class="name">${a.name}</div>
        <div class="status ${a.status}">${a.status} · ${formatBytes(a.size_bytes)}</div>
      </div>
    `).join('');

    // Click to load tileset in viewer
    list.querySelectorAll('.asset-item').forEach(el => {
      el.addEventListener('click', () => loadTileset(el.dataset.id));
    });
  } catch {
    list.innerHTML = '<p style="color:var(--muted);font-size:0.8rem">No assets</p>';
  }
}

// Load a 3D Tileset into the viewer
async function loadTileset(assetId) {
  if (loadedTilesets.has(assetId)) {
    viewer.flyTo(loadedTilesets.get(assetId));
    return;
  }
  try {
    const tileset = await Cesium.Cesium3DTileset.fromUrl(
      `${API}/assets/${assetId}/tileset.json`
    );
    viewer.scene.primitives.add(tileset);
    loadedTilesets.set(assetId, tileset);
    viewer.flyTo(tileset);
  } catch (e) {
    console.error('Failed to load tileset:', e);
  }
}

// Upload file
document.getElementById('upload-btn').addEventListener('click', async () => {
  const input = document.getElementById('file-input');
  const file = input.files[0];
  if (!file) return;

  // Create asset
  const ext = file.name.split('.').pop().toLowerCase();
  const assetType = ['las', 'laz', 'e57', 'ply'].includes(ext) ? 'pointcloud'
    : ['tif', 'tiff', 'hgt'].includes(ext) ? 'terrain'
    : 'model';

  const res = await fetch(`${API}/assets`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ name: file.name, asset_type: assetType }),
  });

  if (res.ok) {
    await loadAssets();
  }
});

// Navigation
document.querySelectorAll('.nav-btn').forEach(btn => {
  btn.addEventListener('click', () => {
    document.querySelectorAll('.nav-btn').forEach(b => b.classList.remove('active'));
    btn.classList.add('active');
  });
});

// Utilities
function formatBytes(bytes) {
  if (!bytes) return '0 B';
  const k = 1024;
  const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return `${(bytes / Math.pow(k, i)).toFixed(1)} ${sizes[i]}`;
}

// Init
checkHealth();
loadAssets();
setInterval(checkHealth, 10000);
setInterval(loadAssets, 5000);

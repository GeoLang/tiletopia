import * as Cesium from 'cesium';
import 'cesium/Build/Cesium/Widgets/widgets.css';
import './style.css';
import { setCesiumViewer, initRendererSelector, getRendererInfo } from './renderers.js';
import { MeasurementTool } from './measurement.js';
import { AnnotationTool } from './annotations.js';
import { FeaturePicker, StyleEditor } from './feature-picker.js';
import { StoryPlayer, fetchStories } from './stories.js';
import { CollaborationPanel } from './collaboration.js';

// API base URL (proxied in dev, same-origin in production)
const API = '/api/v1';

// Initialize Cesium viewer
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
  creditContainer: document.createElement('div'),
  baseLayer: new Cesium.ImageryLayer(
    new Cesium.OpenStreetMapImageryProvider({
      url: 'https://tile.openstreetmap.org/',
    })
  ),
});

// Track loaded tilesets
const loadedTilesets = new Map();

// Wire up multi-renderer
setCesiumViewer(viewer);
initRendererSelector();

// ─── Viewer Tools ────────────────────────────────────────────────────────────

const measureTool = new MeasurementTool(viewer);
const annotationTool = new AnnotationTool(viewer);
const featurePicker = new FeaturePicker(viewer);
const styleEditor = new StyleEditor(viewer, null);
const storyPlayer = new StoryPlayer(viewer);
const collabPanel = new CollaborationPanel(viewer);

// Toolbar button wiring
function deactivateToolbar() {
  document.querySelectorAll('.toolbar-btn').forEach(b => b.classList.remove('active'));
  measureTool.clear();
  annotationTool.disable();
  featurePicker.disable();
  styleEditor.hide();
}

document.getElementById('tb-distance').addEventListener('click', (e) => {
  deactivateToolbar();
  e.currentTarget.classList.add('active');
  measureTool.startDistance();
});

document.getElementById('tb-area').addEventListener('click', (e) => {
  deactivateToolbar();
  e.currentTarget.classList.add('active');
  measureTool.startArea();
});

document.getElementById('tb-height').addEventListener('click', (e) => {
  deactivateToolbar();
  e.currentTarget.classList.add('active');
  measureTool.startHeight();
});

document.getElementById('tb-annotate').addEventListener('click', (e) => {
  deactivateToolbar();
  e.currentTarget.classList.add('active');
  annotationTool.enable();
});

document.getElementById('tb-style').addEventListener('click', () => {
  styleEditor.show();
});

document.getElementById('tb-featureinfo').addEventListener('click', (e) => {
  deactivateToolbar();
  e.currentTarget.classList.add('active');
  featurePicker.enable();
});

document.getElementById('tb-clear').addEventListener('click', () => {
  deactivateToolbar();
  annotationTool.clearAll();
});

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

    // Connect style editor and annotations to the loaded tileset
    styleEditor.setTileset(tileset);
    annotationTool.setAsset(assetId);
    annotationTool.fetchAnnotations();

    // Connect collaboration panel for this asset
    collabPanel.connect(assetId);
  } catch (e) {
    console.error('Failed to load tileset:', e);
  }
}

// Upload file
document.getElementById('upload-btn').addEventListener('click', async () => {
  const input = document.getElementById('file-input');
  const file = input.files[0];
  if (!file) return;

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

// ─── Panel Navigation ────────────────────────────────────────────────────────

const panelMap = {
  viewer: 'cesium-container',
  catalog: 'panel-catalog',
  assets: 'cesium-container',
  jobs: 'cesium-container',
  measure: 'panel-measure',
  anomaly: 'panel-anomaly',
  clash: 'panel-clash',
  admin: 'panel-admin',
  stories: 'panel-stories',
};

document.querySelectorAll('.nav-btn').forEach(btn => {
  btn.addEventListener('click', () => {
    document.querySelectorAll('.nav-btn').forEach(b => b.classList.remove('active'));
    btn.classList.add('active');
    const view = btn.dataset.view;
    document.querySelectorAll('.panel').forEach(p => p.classList.remove('active'));
    const targetId = panelMap[view];
    if (targetId) document.getElementById(targetId).classList.add('active');
    // Load data for panels
    if (view === 'catalog') loadCatalog();
    if (view === 'measure') loadMeasurement();
    if (view === 'anomaly') loadAnomaly();
    if (view === 'clash') loadClash();
    if (view === 'admin') loadAdmin();
    if (view === 'stories') loadStories();
  });
});

// ─── Measurement Panel ───────────────────────────────────────────────────────

async function loadMeasurement() {
  const panel = document.getElementById('panel-measure');
  panel.innerHTML = '<div class="feature-panel"><p style="color:var(--muted)">Loading...</p></div>';
  try {
    const res = await fetch(`${API}/demo/measurement`);
    const d = await res.json();
    panel.innerHTML = `<div class="feature-panel">
      <h2>📏 Measurement Tools</h2>
      <p class="subtitle">Real-time 3D measurements computed from survey data</p>
      <div class="card-grid">
        <div class="metric-card"><div class="label">3D Distance</div><div class="value">${d.distance_m}<span class="unit"> m</span></div></div>
        <div class="metric-card"><div class="label">Polyline Length</div><div class="value">${d.polyline_length_m}<span class="unit"> m</span></div></div>
        <div class="metric-card"><div class="label">Polygon Area</div><div class="value">${d.area_m2}<span class="unit"> m²</span></div></div>
        <div class="metric-card"><div class="label">Mesh Volume</div><div class="value">${d.volume_m3}<span class="unit"> m³</span></div></div>
      </div>
      <h3 class="section-title">Earthwork Analysis</h3>
      <div class="card-grid">
        <div class="metric-card"><div class="label">Cut Volume</div><div class="value">${d.cut_volume_m3}<span class="unit"> m³</span></div></div>
        <div class="metric-card"><div class="label">Fill Volume</div><div class="value">${d.fill_volume_m3}<span class="unit"> m³</span></div></div>
        <div class="metric-card"><div class="label">Slope</div><div class="value">${d.slope_percent}<span class="unit"> %</span></div></div>
        <div class="metric-card"><div class="label">Bearing</div><div class="value">${d.bearing_degrees}<span class="unit"> °</span></div></div>
      </div>
    </div>`;
  } catch(e) {
    panel.innerHTML = `<div class="feature-panel"><p style="color:#f85149">Error: ${e.message}</p></div>`;
  }
}

// ─── Anomaly Detection Panel ─────────────────────────────────────────────────

async function loadAnomaly() {
  const panel = document.getElementById('panel-anomaly');
  panel.innerHTML = '<div class="feature-panel"><p style="color:var(--muted)">Loading...</p></div>';
  try {
    const res = await fetch(`${API}/demo/anomaly`);
    const d = await res.json();
    panel.innerHTML = `<div class="feature-panel">
      <h2>⚠️ Anomaly Detection</h2>
      <p class="subtitle">AI-powered structural monitoring & change detection</p>
      <div class="card-grid">
        <div class="metric-card"><div class="label">Deformation Alerts</div><div class="value">${d.deformation_alerts.length}</div></div>
        <div class="metric-card"><div class="label">Encroachment Zones</div><div class="value">${d.encroachment_alerts.length}</div></div>
        <div class="metric-card"><div class="label">Outliers Removed</div><div class="value">${d.outlier_stats.removed}<span class="unit"> / ${d.outlier_stats.total_points}</span></div></div>
        <div class="metric-card"><div class="label">Z-Score Threshold</div><div class="value">${d.outlier_stats.z_threshold}σ</div></div>
      </div>
      <h3 class="section-title">Deformation Alerts</h3>
      <table class="data-table">
        <thead><tr><th>Grid Cell</th><th>Delta</th><th>Severity</th></tr></thead>
        <tbody>${d.deformation_alerts.slice(0,10).map(a => `<tr>
          <td>[${a.grid_cell[0]}, ${a.grid_cell[1]}]</td>
          <td>${a.delta_m} m</td>
          <td><span class="badge ${a.severity === 'HIGH' ? 'badge-critical' : 'badge-warning'}">${a.severity}</span></td>
        </tr>`).join('')}</tbody>
      </table>
      <h3 class="section-title">Encroachment Zones</h3>
      <table class="data-table">
        <thead><tr><th>Zone</th><th>Points in Buffer</th><th>Min Distance</th></tr></thead>
        <tbody>${d.encroachment_alerts.map(a => `<tr>
          <td>${a.zone_name}</td>
          <td>${a.points_in_buffer}</td>
          <td>${a.min_distance_m} m</td>
        </tr>`).join('')}</tbody>
      </table>
    </div>`;
  } catch(e) {
    panel.innerHTML = `<div class="feature-panel"><p style="color:#f85149">Error: ${e.message}</p></div>`;
  }
}

// ─── Clash Detection Panel ───────────────────────────────────────────────────

async function loadClash() {
  const panel = document.getElementById('panel-clash');
  panel.innerHTML = '<div class="feature-panel"><p style="color:var(--muted)">Loading...</p></div>';
  try {
    const res = await fetch(`${API}/demo/clash`);
    const d = await res.json();
    panel.innerHTML = `<div class="feature-panel">
      <h2>💥 Clash Analytics</h2>
      <p class="subtitle">BIM vs reality clash detection across ${d.total_elements} elements</p>
      <div class="card-grid">
        <div class="metric-card"><div class="label">Hard Clashes</div><div class="value" style="color:#f85149">${d.hard_count}</div></div>
        <div class="metric-card"><div class="label">Soft Clashes</div><div class="value" style="color:#d29922">${d.soft_count}</div></div>
        <div class="metric-card"><div class="label">Total Elements</div><div class="value">${d.total_elements}</div></div>
        <div class="metric-card"><div class="label">Total Clashes</div><div class="value">${d.clashes.length}</div></div>
      </div>
      <h3 class="section-title">Clash Details</h3>
      <table class="data-table">
        <thead><tr><th>Type</th><th>Element A</th><th>Element B</th><th>Detail</th><th>Severity</th></tr></thead>
        <tbody>${d.clashes.map(c => `<tr>
          <td><span class="badge ${c.clash_type === 'HARD' ? 'badge-critical' : 'badge-warning'}">${c.clash_type}</span></td>
          <td>${c.element_a}</td>
          <td>${c.element_b}</td>
          <td>${c.detail}</td>
          <td>${c.severity}</td>
        </tr>`).join('')}</tbody>
      </table>
    </div>`;
  } catch(e) {
    panel.innerHTML = `<div class="feature-panel"><p style="color:#f85149">Error: ${e.message}</p></div>`;
  }
}

// ─── Enterprise Admin Panel ──────────────────────────────────────────────────

async function loadAdmin() {
  const panel = document.getElementById('panel-admin');
  panel.innerHTML = '<div class="feature-panel"><p style="color:var(--muted)">Loading...</p></div>';
  try {
    const [auditRes, rbacRes] = await Promise.all([
      fetch(`${API}/demo/audit`),
      fetch(`${API}/demo/rbac`),
    ]);
    const audit = await auditRes.json();
    const rbac = await rbacRes.json();
    panel.innerHTML = `<div class="feature-panel">
      <h2>🔒 Enterprise Admin</h2>
      <p class="subtitle">RBAC, OIDC SSO, and full audit trail — Provider: ${rbac.provider}</p>
      <h3 class="section-title">Users & Roles</h3>
      <table class="data-table">
        <thead><tr><th>Email</th><th>Role</th></tr></thead>
        <tbody>${rbac.users.map(u => `<tr>
          <td>${u.email}</td>
          <td><span class="badge badge-info">${u.role}</span></td>
        </tr>`).join('')}</tbody>
      </table>
      <h3 class="section-title">Audit Trail (last ${audit.length} events)</h3>
      <table class="data-table">
        <thead><tr><th>Time</th><th>User</th><th>Action</th><th>Resource</th><th>Status</th></tr></thead>
        <tbody>${audit.map(e => `<tr>
          <td style="white-space:nowrap">${new Date(e.timestamp).toLocaleString()}</td>
          <td>${e.user_id}</td>
          <td>${e.action}</td>
          <td>${e.resource_type}/${e.resource_id}</td>
          <td><span class="badge ${e.success ? 'badge-success' : 'badge-critical'}">${e.success ? 'OK' : 'DENIED'}</span></td>
        </tr>`).join('')}</tbody>
      </table>
    </div>`;
  } catch(e) {
    panel.innerHTML = `<div class="feature-panel"><p style="color:#f85149">Error: ${e.message}</p></div>`;
  }
}

// ─── Stories Panel ───────────────────────────────────────────────────────────

async function loadStories() {
  const panel = document.getElementById('panel-stories');
  panel.innerHTML = '<div class="feature-panel"><p style="color:var(--muted)">Loading...</p></div>';
  try {
    let stories;
    const apiRes = await fetch(`${API}/stories`);
    if (apiRes.ok) {
      stories = await apiRes.json();
    } else {
      const demoRes = await fetch(`${API}/demo/stories`);
      stories = await demoRes.json();
    }
    panel.innerHTML = `<div class="feature-panel">
      <h2>🎬 Narrated Presentations (Stories)</h2>
      <p class="subtitle">Cinematic 3D walkthroughs with camera paths and narration</p>
      ${stories.map((s, idx) => `<div class="story-card" data-story-idx="${idx}">
        <h3>${s.title}</h3>
        <div class="meta">${s.description} · ${(s.slides || []).length} slides</div>
        <div class="slide-list">
          ${(s.slides || []).map((sl,i) => `<span class="slide-chip">${i+1}. ${sl.title || 'Untitled'}</span>`).join('')}
        </div>
        <button class="play-story-btn" data-story-idx="${idx}">▶ Play</button>
      </div>`).join('')}
    </div>`;

    panel.querySelectorAll('.play-story-btn').forEach(btn => {
      btn.addEventListener('click', () => {
        const i = parseInt(btn.dataset.storyIdx);
        storyPlayer.load(stories[i]);
        storyPlayer.play();
        // Switch back to viewer
        document.querySelectorAll('.nav-btn').forEach(b => b.classList.remove('active'));
        document.querySelectorAll('.panel').forEach(p => p.classList.remove('active'));
        document.getElementById('cesium-container').classList.add('active');
      });
    });
  } catch(e) {
    panel.innerHTML = `<div class="feature-panel"><p style="color:#f85149">Error: ${e.message}</p></div>`;
  }
}

// ─── Data Catalog Panel ──────────────────────────────────────────────────────

async function loadCatalog() {
  const panel = document.getElementById('panel-catalog');
  panel.innerHTML = '<div class="feature-panel"><p style="color:var(--muted)">Loading...</p></div>';
  try {
    const res = await fetch(`${API}/catalog`);
    const datasets = await res.json();
    const renderers = getRendererInfo();

    const categories = [...new Set(datasets.map(d => d.category))];
    const grouped = {};
    for (const cat of categories) {
      grouped[cat] = datasets.filter(d => d.category === cat);
    }

    const categoryIcons = {
      Terrain: '⛰️', Buildings: '🏢', Imagery: '🛰️',
      PointCloud: '📍', Vector: '🗺️', Weather: '🌤️'
    };

    panel.innerHTML = `<div class="feature-panel">
      <h2>📦 Open Data Catalog</h2>
      <p class="subtitle">Curated open geospatial datasets — no subscriptions required. ${datasets.length} datasets across ${categories.length} categories.</p>

      <div class="card-grid">
        <div class="metric-card"><div class="label">Total Datasets</div><div class="value">${datasets.length}</div></div>
        <div class="metric-card"><div class="label">Categories</div><div class="value">${categories.length}</div></div>
        <div class="metric-card"><div class="label">Renderers</div><div class="value">${renderers.length}</div></div>
        <div class="metric-card"><div class="label">Free / Open</div><div class="value">${datasets.filter(d => d.enabled).length}</div></div>
      </div>

      <h3 class="section-title">Multi-Renderer Support</h3>
      <div class="renderer-grid">
        ${renderers.map(r => `<div class="renderer-card">
          <h4>${r.name}</h4>
          <p>${r.description}</p>
          <div class="feature-tags">${r.features.map(f => `<span class="tag">${f}</span>`).join('')}</div>
        </div>`).join('')}
      </div>

      ${categories.map(cat => `
        <h3 class="section-title">${categoryIcons[cat] || '📁'} ${cat}</h3>
        <table class="data-table">
          <thead><tr><th>Dataset</th><th>Provider</th><th>Format</th><th>Resolution</th><th>Coverage</th><th>Status</th></tr></thead>
          <tbody>${grouped[cat].map(d => `<tr>
            <td><strong>${d.name}</strong><br><span style="color:var(--muted);font-size:0.75rem">${d.description.slice(0,80)}…</span></td>
            <td>${d.provider}</td>
            <td><span class="badge">${d.format}</span></td>
            <td>${d.resolution || '—'}</td>
            <td>${d.coverage.scope === 'Global' ? '🌍 Global' : d.coverage.scope.Regional || d.coverage.scope.National || 'Local'}</td>
            <td>${d.enabled ? '<span class="badge badge-success">Ready</span>' : '<span class="badge badge-warning">API Key</span>'}</td>
          </tr>`).join('')}</tbody>
        </table>
      `).join('')}
    </div>`;
  } catch(e) {
    panel.innerHTML = `<div class="feature-panel"><p style="color:#f85149">Error: ${e.message}</p></div>`;
  }
}

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

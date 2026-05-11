/**
 * GeoLang Agent Chat — connects to Letta-powered geospatial agent.
 *
 * Sends natural language queries and receives:
 * - Text responses
 * - Viewer commands (fly to, show layer, classify, etc.)
 * - Map/table/image UI specs
 */
import * as Cesium from 'cesium';

const GEOLANG_URL = '/agent'; // proxied to GeoLang server

/** Viewer command handlers — the agent can invoke these. */
const viewerCommands = {};

/**
 * Register the viewer instance for agent control.
 */
export function initAgentChat(viewer, opts = {}) {
  const geolangUrl = opts.geolangUrl ?? GEOLANG_URL;
  const messagesEl = document.getElementById('chat-messages');
  const inputEl = document.getElementById('chat-input');
  const sendBtn = document.getElementById('chat-send');
  const statusEl = document.getElementById('agent-status');
  const toggleBtn = document.getElementById('chat-toggle');
  const panel = document.getElementById('chat-panel');

  if (!messagesEl || !inputEl) return;

  // Toggle panel
  toggleBtn?.addEventListener('click', () => {
    panel.classList.toggle('collapsed');
    toggleBtn.textContent = panel.classList.contains('collapsed') ? '▶' : '◀';
  });

  // Register viewer commands the agent can use
  registerViewerCommands(viewer);

  // Send message
  async function sendMessage() {
    const text = inputEl.value.trim();
    if (!text) return;

    addMessage(text, 'user');
    inputEl.value = '';
    statusEl.textContent = 'Thinking...';
    sendBtn.disabled = true;

    try {
      const response = await fetch(`${geolangUrl}/chat/stream`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ message: text }),
      });

      if (!response.ok) {
        throw new Error(`Agent error: ${response.status}`);
      }

      // Handle SSE stream
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = '';
      let lastText = '';

      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });

        const lines = buffer.split('\n');
        buffer = lines.pop(); // keep incomplete line

        for (const line of lines) {
          if (!line.startsWith('data: ')) continue;
          try {
            const event = JSON.parse(line.slice(6));
            if (event.type === 'progress') {
              statusEl.textContent = event.text;
            } else if (event.type === 'text') {
              lastText = event.text;
            } else if (event.type === 'ui_spec' && event.spec) {
              handleUiSpec(event.spec, viewer);
            } else if (event.type === 'viewer_cmd') {
              executeViewerCommand(event.cmd, viewer);
            } else if (event.type === 'done') {
              if (lastText) addMessage(lastText, 'agent');
            } else if (event.type === 'error') {
              addMessage(`Error: ${event.text}`, 'agent');
            }
          } catch { /* ignore parse errors */ }
        }
      }

      statusEl.textContent = 'Ready';
    } catch (e) {
      addMessage(`Error: ${e.message}`, 'agent');
      statusEl.textContent = 'Error — check GeoLang server';
    } finally {
      sendBtn.disabled = false;
    }
  }

  sendBtn.addEventListener('click', sendMessage);
  inputEl.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      sendMessage();
    }
  });

  // Welcome message
  addMessage(
    'Hello! I\'m the GeoLang agent. I can help you analyze point clouds, ' +
    'fly to locations, show layers, run ML classification, and more. ' +
    'Try: "Fly to London" or "Classify the loaded point cloud"',
    'agent',
  );
}

function addMessage(text, role) {
  const messagesEl = document.getElementById('chat-messages');
  const msg = document.createElement('div');
  msg.className = `chat-msg ${role}`;
  msg.textContent = text;
  messagesEl.appendChild(msg);
  messagesEl.scrollTop = messagesEl.scrollHeight;
}

function addMessageHtml(html, role) {
  const messagesEl = document.getElementById('chat-messages');
  const msg = document.createElement('div');
  msg.className = `chat-msg ${role}`;
  msg.innerHTML = html;
  messagesEl.appendChild(msg);
  messagesEl.scrollTop = messagesEl.scrollHeight;
}

/**
 * Handle agent response — may include text, viewer commands, and UI specs.
 */
function handleAgentResponse(data, viewer) {
  // Text response (GeoLang uses 'text', fallback to 'response')
  const text = data.text || data.response;
  if (text) {
    addMessage(text, 'agent');
  }

  // Viewer commands
  if (data.viewer_commands) {
    for (const cmd of data.viewer_commands) {
      executeViewerCommand(cmd, viewer);
    }
  }

  // UI spec (map layers, tables, images)
  if (data.ui_spec) {
    handleUiSpec(data.ui_spec, viewer);
  }
}

/**
 * Register viewer commands that the agent can invoke.
 */
function registerViewerCommands(viewer) {
  viewerCommands.fly_to = (params) => {
    const { lon, lat, height = 1000, duration = 2 } = params;
    viewer.camera.flyTo({
      destination: Cesium.Cartesian3.fromDegrees(lon, lat, height),
      duration,
    });
    addMessage(`📍 Flying to ${lat.toFixed(4)}, ${lon.toFixed(4)}`, 'agent');
  };

  viewerCommands.set_view = (params) => {
    const { lon, lat, height = 5000, heading = 0, pitch = -45, roll = 0 } = params;
    viewer.camera.setView({
      destination: Cesium.Cartesian3.fromDegrees(lon, lat, height),
      orientation: {
        heading: Cesium.Math.toRadians(heading),
        pitch: Cesium.Math.toRadians(pitch),
        roll: Cesium.Math.toRadians(roll),
      },
    });
  };

  viewerCommands.add_marker = (params) => {
    const { lon, lat, label, color = '#ff0000' } = params;
    viewer.entities.add({
      position: Cesium.Cartesian3.fromDegrees(lon, lat),
      point: { pixelSize: 10, color: Cesium.Color.fromCssColorString(color) },
      label: label
        ? { text: label, font: '14px sans-serif', verticalOrigin: Cesium.VerticalOrigin.BOTTOM, pixelOffset: new Cesium.Cartesian2(0, -12) }
        : undefined,
    });
  };

  viewerCommands.clear_entities = () => {
    viewer.entities.removeAll();
    addMessage('🗑️ Cleared all entities', 'agent');
  };

  viewerCommands.load_tileset = async (params) => {
    const { url, label } = params;
    try {
      const tileset = await Cesium.Cesium3DTileset.fromUrl(url);
      viewer.scene.primitives.add(tileset);
      viewer.flyTo(tileset);
      addMessage(`📦 Loaded tileset: ${label || url}`, 'agent');
    } catch (e) {
      addMessage(`Failed to load tileset: ${e.message}`, 'agent');
    }
  };

  viewerCommands.classify = (params) => {
    const { attribute = 'Classification' } = params;
    // Import dynamically to avoid circular deps
    import('./classification-viz.js').then(({ applyClassificationStyle }) => {
      for (const prim of viewer.scene.primitives) {
        if (prim instanceof Cesium.Cesium3DTileset) {
          applyClassificationStyle(prim, attribute);
        }
      }
      addMessage('🎯 Applied classification coloring', 'agent');
    });
  };

  viewerCommands.add_geojson = async (params) => {
    const { url, color = '#3388ff', label } = params;
    try {
      const ds = await Cesium.GeoJsonDataSource.load(url, {
        stroke: Cesium.Color.fromCssColorString(color),
        fill: Cesium.Color.fromCssColorString(color).withAlpha(0.3),
        strokeWidth: 2,
      });
      viewer.dataSources.add(ds);
      viewer.flyTo(ds);
      addMessage(`🗺️ Loaded layer: ${label || url}`, 'agent');
    } catch (e) {
      addMessage(`Failed to load GeoJSON: ${e.message}`, 'agent');
    }
  };

  viewerCommands.set_time = (params) => {
    const { iso } = params;
    viewer.clock.currentTime = Cesium.JulianDate.fromIso8601(iso);
    addMessage(`⏱️ Time set to ${iso}`, 'agent');
  };

  viewerCommands.screenshot = () => {
    viewer.render();
    const canvas = viewer.canvas;
    canvas.toBlob((blob) => {
      const url = URL.createObjectURL(blob);
      addMessageHtml(`<img src="${url}" style="max-width:100%;border-radius:4px">`, 'agent');
    });
  };
}

function executeViewerCommand(cmd, viewer) {
  const handler = viewerCommands[cmd.action];
  if (handler) {
    handler(cmd.params || {});
  } else {
    addMessage(`Unknown command: ${cmd.action}`, 'agent');
  }
}

function handleUiSpec(spec, viewer) {
  if (spec.ui_type === 'map' && spec.layers) {
    // Load each layer as GeoJSON
    for (const layer of spec.layers) {
      executeViewerCommand({
        action: 'add_geojson',
        params: { url: layer.path, color: layer.color, label: layer.name },
      }, viewer);
    }
    if (spec.center_lon != null && spec.center_lat != null) {
      executeViewerCommand({
        action: 'fly_to',
        params: { lon: spec.center_lon, lat: spec.center_lat, height: 5000 },
      }, viewer);
    }
  } else if (spec.ui_type === 'table') {
    let html = `<strong>${spec.title || 'Results'}</strong><table style="font-size:0.8rem;margin-top:4px;border-collapse:collapse">`;
    if (spec.columns) {
      html += '<tr>' + spec.columns.map((c) => `<th style="padding:2px 6px;border-bottom:1px solid #444">${c}</th>`).join('') + '</tr>';
    }
    if (spec.rows) {
      for (const row of spec.rows) {
        html += '<tr>' + row.map((c) => `<td style="padding:2px 6px">${c}</td>`).join('') + '</tr>';
      }
    }
    html += '</table>';
    addMessageHtml(html, 'agent');
  } else if (spec.ui_type === 'image' && spec.image_path) {
    addMessageHtml(`<img src="${spec.image_path}" style="max-width:100%;border-radius:4px">`, 'agent');
  }
}

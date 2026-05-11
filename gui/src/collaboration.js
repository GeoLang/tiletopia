/**
 * Collaboration panel — real-time presence, cursor sharing, and chat over WebSocket.
 */
import * as Cesium from 'cesium';

export class CollaborationPanel {
  constructor(viewer) {
    this.viewer = viewer;
    this.ws = null;
    this.assetId = null;
    this.userId = 'user-' + Math.random().toString(36).slice(2, 8);
    this.userName = 'Anonymous';
    this.cursorEntities = new Map();
    this.onPresenceUpdate = null;
    this.onChatMessage = null;

    this._createPanel();
    this._trackCursor();
  }

  connect(assetId) {
    this.disconnect();
    this.assetId = assetId;

    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${proto}//${location.host}/api/v1/realtime/${assetId}`;
    this.ws = new WebSocket(url);

    this.ws.addEventListener('open', () => {
      this._send({
        type: 'Join',
        user_id: this.userId,
        asset_id: assetId,
        user_name: this.userName,
      });
      this._showPanel(true);
    });

    this.ws.addEventListener('message', (evt) => {
      try {
        const msg = JSON.parse(evt.data);
        this._handleMessage(msg);
      } catch { /* ignore non-JSON */ }
    });

    this.ws.addEventListener('close', () => {
      this._clearCursors();
    });
  }

  disconnect() {
    if (this.ws) {
      if (this.assetId) {
        this._send({
          type: 'Leave',
          user_id: this.userId,
          asset_id: this.assetId,
        });
      }
      this.ws.close();
      this.ws = null;
    }
    this._clearCursors();
    this._showPanel(false);
    this.assetId = null;
  }

  sendCursor(position) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    this._send({
      type: 'Cursor',
      user_id: this.userId,
      longitude: position.longitude,
      latitude: position.latitude,
      height: position.height || 0,
    });
  }

  sendChat(message) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    this._send({
      type: 'Chat',
      user_id: this.userId,
      user_name: this.userName,
      message,
      timestamp: new Date().toISOString(),
    });
  }

  // ─── Private ─────────────────────────────────────────────────────────

  _send(msg) {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(msg));
    }
  }

  _handleMessage(msg) {
    switch (msg.type) {
      case 'Presence':
        this._updatePresenceList(msg.users || []);
        if (this.onPresenceUpdate) this.onPresenceUpdate(msg.users);
        break;

      case 'Cursor':
        if (msg.user_id !== this.userId) {
          this._updateCursorEntity(msg);
        }
        break;

      case 'Chat':
        this._appendChat(msg);
        if (this.onChatMessage) this.onChatMessage(msg);
        break;

      case 'ViewChanged':
        break;
    }
  }

  _updatePresenceList(users) {
    const list = document.getElementById('collab-users');
    if (!list) return;
    list.innerHTML = users.map(u =>
      `<div class="collab-user">
        <span class="collab-dot" style="background:${u.color}"></span>
        ${u.user_name}${u.user_id === this.userId ? ' (you)' : ''}
      </div>`
    ).join('');
  }

  _updateCursorEntity(msg) {
    const existing = this.cursorEntities.get(msg.user_id);
    const pos = Cesium.Cartesian3.fromDegrees(msg.longitude, msg.latitude, msg.height || 0);

    if (existing) {
      existing.position = pos;
    } else {
      const entity = this.viewer.entities.add({
        position: pos,
        point: {
          pixelSize: 10,
          color: Cesium.Color.YELLOW,
          outlineColor: Cesium.Color.WHITE,
          outlineWidth: 2,
          disableDepthTestDistance: Number.POSITIVE_INFINITY,
        },
        label: {
          text: msg.user_id,
          font: '12px sans-serif',
          fillColor: Cesium.Color.WHITE,
          pixelOffset: new Cesium.Cartesian2(0, -18),
          disableDepthTestDistance: Number.POSITIVE_INFINITY,
        },
      });
      this.cursorEntities.set(msg.user_id, entity);
    }
  }

  _clearCursors() {
    for (const entity of this.cursorEntities.values()) {
      this.viewer.entities.remove(entity);
    }
    this.cursorEntities.clear();
  }

  _appendChat(msg) {
    const log = document.getElementById('collab-chat-log');
    if (!log) return;
    const div = document.createElement('div');
    div.className = 'chat-msg';
    div.innerHTML = `<strong>${msg.user_name}:</strong> ${this._escapeHtml(msg.message)}`;
    log.appendChild(div);
    log.scrollTop = log.scrollHeight;
  }

  _escapeHtml(text) {
    const div = document.createElement('div');
    div.textContent = text;
    return div.innerHTML;
  }

  _trackCursor() {
    let throttle = 0;
    const handler = new Cesium.ScreenSpaceEventHandler(this.viewer.scene.canvas);
    handler.setInputAction((movement) => {
      const now = Date.now();
      if (now - throttle < 200) return;
      throttle = now;

      const ray = this.viewer.camera.getPickRay(movement.endPosition);
      if (!ray) return;
      const cartesian = this.viewer.scene.globe.pick(ray, this.viewer.scene);
      if (!cartesian) return;

      const carto = Cesium.Cartographic.fromCartesian(cartesian);
      this.sendCursor({
        longitude: Cesium.Math.toDegrees(carto.longitude),
        latitude: Cesium.Math.toDegrees(carto.latitude),
        height: carto.height,
      });
    }, Cesium.ScreenSpaceEventType.MOUSE_MOVE);
  }

  _createPanel() {
    const panel = document.createElement('div');
    panel.id = 'collab-panel';
    panel.className = 'collab-panel';
    panel.style.display = 'none';
    panel.innerHTML = `
      <div class="collab-header">
        <span>Collaboration</span>
        <button id="collab-close">✕</button>
      </div>
      <div id="collab-users" class="collab-users"></div>
      <div id="collab-chat-log" class="collab-chat-log"></div>
      <div class="collab-chat-input">
        <input id="collab-chat-text" type="text" placeholder="Type a message..." />
        <button id="collab-send">Send</button>
      </div>
    `;
    document.body.appendChild(panel);

    panel.querySelector('#collab-close').addEventListener('click', () => this.disconnect());
    panel.querySelector('#collab-send').addEventListener('click', () => {
      const input = document.getElementById('collab-chat-text');
      if (input.value.trim()) {
        this.sendChat(input.value.trim());
        input.value = '';
      }
    });
    panel.querySelector('#collab-chat-text').addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        const input = e.target;
        if (input.value.trim()) {
          this.sendChat(input.value.trim());
          input.value = '';
        }
      }
    });
  }

  _showPanel(visible) {
    const panel = document.getElementById('collab-panel');
    if (panel) panel.style.display = visible ? 'flex' : 'none';
  }
}

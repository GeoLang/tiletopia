import * as Cesium from 'cesium';

/**
 * Feature info picking — shows metadata when clicking 3D Tiles features.
 */
export class FeaturePicker {
  constructor(viewer) {
    this.viewer = viewer;
    this.handler = new Cesium.ScreenSpaceEventHandler(viewer.scene.canvas);
    this.panelEl = null;
    this.enabled = false;
    this.highlighted = null;
    this._originalColor = null;
    this._init();
  }

  _init() {
    // Create the info panel element
    this.panelEl = document.createElement('div');
    this.panelEl.id = 'feature-info-panel';
    this.panelEl.className = 'feature-info-panel';
    this.panelEl.style.display = 'none';
    this.panelEl.innerHTML = '<div class="fip-header"><span>Feature Info</span><button class="fip-close">&times;</button></div><div class="fip-body"></div>';
    document.getElementById('main-content').appendChild(this.panelEl);

    this.panelEl.querySelector('.fip-close').addEventListener('click', () => this.hidePanel());
  }

  /** Enable feature picking on left click. */
  enable() {
    if (this.enabled) return;
    this.enabled = true;

    this.handler.setInputAction((click) => {
      this._clearHighlight();

      const picked = this.viewer.scene.pick(click.position);
      if (Cesium.defined(picked) && picked instanceof Cesium.Cesium3DTileFeature) {
        this._showFeatureInfo(picked);
        this._highlight(picked);
      } else {
        this.hidePanel();
      }
    }, Cesium.ScreenSpaceEventType.LEFT_CLICK);
  }

  /** Disable feature picking. */
  disable() {
    this.enabled = false;
    this.handler.removeInputAction(Cesium.ScreenSpaceEventType.LEFT_CLICK);
    this._clearHighlight();
    this.hidePanel();
  }

  hidePanel() {
    if (this.panelEl) this.panelEl.style.display = 'none';
  }

  _showFeatureInfo(feature) {
    const propertyIds = feature.getPropertyIds();
    if (propertyIds.length === 0) {
      this.panelEl.querySelector('.fip-body').innerHTML = '<p class="fip-empty">No properties</p>';
    } else {
      const rows = propertyIds.map((id) => {
        const val = feature.getProperty(id);
        const display = typeof val === 'object' ? JSON.stringify(val) : String(val);
        return `<tr><td class="fip-key">${this._escapeHtml(id)}</td><td class="fip-val">${this._escapeHtml(display)}</td></tr>`;
      }).join('');
      this.panelEl.querySelector('.fip-body').innerHTML = `<table class="fip-table">${rows}</table>`;
    }
    this.panelEl.style.display = 'block';
  }

  _highlight(feature) {
    this._originalColor = feature.color ? Cesium.Color.clone(feature.color) : null;
    feature.color = Cesium.Color.YELLOW.withAlpha(0.6);
    this.highlighted = feature;
  }

  _clearHighlight() {
    if (this.highlighted) {
      if (this._originalColor) {
        this.highlighted.color = this._originalColor;
      } else {
        this.highlighted.color = Cesium.Color.WHITE;
      }
      this.highlighted = null;
      this._originalColor = null;
    }
  }

  _escapeHtml(str) {
    const div = document.createElement('div');
    div.textContent = str;
    return div.innerHTML;
  }
}

/**
 * Style editor — colour 3D Tiles features by property, height, or classification.
 */
export class StyleEditor {
  constructor(viewer, tileset) {
    this.viewer = viewer;
    this.tileset = tileset;
    this.panelEl = null;
    this._init();
  }

  _init() {
    this.panelEl = document.createElement('div');
    this.panelEl.id = 'style-editor-panel';
    this.panelEl.className = 'style-editor-panel';
    this.panelEl.style.display = 'none';
    this.panelEl.innerHTML = `
      <div class="sep-header"><span>🎨 Style Editor</span><button class="sep-close">&times;</button></div>
      <div class="sep-body">
        <div class="sep-section">
          <label>Color by Property</label>
          <input type="text" id="sep-prop-name" placeholder="Property name" class="sep-input">
          <button class="sep-btn" id="sep-apply-prop">Apply</button>
        </div>
        <div class="sep-section">
          <button class="sep-btn" id="sep-color-height">Color by Height</button>
          <button class="sep-btn" id="sep-color-class">Color by Classification</button>
          <button class="sep-btn sep-btn-reset" id="sep-reset">Reset Style</button>
        </div>
      </div>`;
    document.getElementById('main-content').appendChild(this.panelEl);

    this.panelEl.querySelector('.sep-close').addEventListener('click', () => this.hide());
    this.panelEl.querySelector('#sep-apply-prop').addEventListener('click', () => {
      const prop = this.panelEl.querySelector('#sep-prop-name').value.trim();
      if (prop) this.setColorByProperty(prop);
    });
    this.panelEl.querySelector('#sep-color-height').addEventListener('click', () => this.setColorByHeight());
    this.panelEl.querySelector('#sep-color-class').addEventListener('click', () => this.setColorByClassification());
    this.panelEl.querySelector('#sep-reset').addEventListener('click', () => this.resetStyle());
  }

  show() {
    if (this.panelEl) this.panelEl.style.display = 'block';
  }

  hide() {
    if (this.panelEl) this.panelEl.style.display = 'none';
  }

  /** Assign the tileset to style (call when a new tileset loads). */
  setTileset(tileset) {
    this.tileset = tileset;
  }

  /** Colour features by a string/numeric property using a hash-based palette. */
  setColorByProperty(propertyName) {
    if (!this.tileset) return;
    this.tileset.style = new Cesium.Cesium3DTileStyle({
      color: {
        conditions: [
          [`\${${propertyName}} === undefined`, 'color("gray")'],
          ['true', `color("hsl(" + (\${${propertyName}} * 137.508 % 360) + ", 70%, 55%)")`],
        ],
      },
    });
  }

  /** Colour features by height using a gradient. */
  setColorByHeight() {
    if (!this.tileset) return;
    this.tileset.style = new Cesium.Cesium3DTileStyle({
      color: {
        conditions: [
          ['${height} > 200', 'color("#d73027")'],
          ['${height} > 150', 'color("#fc8d59")'],
          ['${height} > 100', 'color("#fee08b")'],
          ['${height} > 50', 'color("#d9ef8b")'],
          ['${height} > 20', 'color("#91cf60")'],
          ['${height} > 0', 'color("#1a9850")'],
          ['true', 'color("gray")'],
        ],
      },
    });
  }

  /** Colour features by classification code. */
  setColorByClassification() {
    if (!this.tileset) return;
    this.tileset.style = new Cesium.Cesium3DTileStyle({
      color: {
        conditions: [
          ['${classification} === 2', 'color("#8B4513")'],  // Ground
          ['${classification} === 3', 'color("#228B22")'],  // Low Vegetation
          ['${classification} === 4', 'color("#006400")'],  // Medium Vegetation
          ['${classification} === 5', 'color("#013220")'],  // High Vegetation
          ['${classification} === 6', 'color("#FF4500")'],  // Building
          ['${classification} === 9', 'color("#1E90FF")'],  // Water
          ['true', 'color("gray")'],
        ],
      },
    });
  }

  /** Remove any custom style. */
  resetStyle() {
    if (!this.tileset) return;
    this.tileset.style = undefined;
  }
}

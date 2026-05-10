import * as Cesium from 'cesium';

/**
 * Interactive measurement tools for the CesiumJS viewer.
 * Supports distance, area, and height measurements.
 */
export class MeasurementTool {
  constructor(viewer) {
    this.viewer = viewer;
    this.handler = new Cesium.ScreenSpaceEventHandler(viewer.scene.canvas);
    this.activeMode = null;
    this.positions = [];
    this.entities = [];
    this._onComplete = null;
  }

  /** Start distance measurement — click two points. */
  startDistance() {
    this.clear();
    this.activeMode = 'distance';
    this.positions = [];
    this._setStatus('Click first point');

    this.handler.setInputAction((click) => {
      const ray = this.viewer.camera.getPickRay(click.position);
      const pos = this.viewer.scene.globe.pick(ray, this.viewer.scene);
      if (!Cesium.defined(pos)) return;

      this.positions.push(pos);

      // Mark the point
      this._addPointEntity(pos);

      if (this.positions.length === 1) {
        this._setStatus('Click second point');
      } else if (this.positions.length === 2) {
        const dist = Cesium.Cartesian3.distance(this.positions[0], this.positions[1]);
        this._drawLine(this.positions[0], this.positions[1]);
        this._addLabel(
          Cesium.Cartesian3.midpoint(this.positions[0], this.positions[1], new Cesium.Cartesian3()),
          this._formatDistance(dist)
        );
        this._setStatus(`Distance: ${this._formatDistance(dist)}`);
        this._stopInput();
      }
    }, Cesium.ScreenSpaceEventType.LEFT_CLICK);
  }

  /** Start area measurement — click polygon points, double-click to finish. */
  startArea() {
    this.clear();
    this.activeMode = 'area';
    this.positions = [];
    this._setStatus('Click polygon points (double-click to finish)');

    this.handler.setInputAction((click) => {
      const ray = this.viewer.camera.getPickRay(click.position);
      const pos = this.viewer.scene.globe.pick(ray, this.viewer.scene);
      if (!Cesium.defined(pos)) return;

      this.positions.push(pos);
      this._addPointEntity(pos);

      if (this.positions.length > 1) {
        this._drawLine(this.positions[this.positions.length - 2], pos);
      }

      this._setStatus(`${this.positions.length} points — double-click to finish`);
    }, Cesium.ScreenSpaceEventType.LEFT_CLICK);

    this.handler.setInputAction(() => {
      if (this.positions.length < 3) {
        this._setStatus('Need at least 3 points');
        return;
      }
      // Close the polygon
      this._drawLine(this.positions[this.positions.length - 1], this.positions[0]);
      this._drawPolygon(this.positions);

      const area = this._computeArea(this.positions);
      const centroid = this._computeCentroid(this.positions);
      this._addLabel(centroid, this._formatArea(area));
      this._setStatus(`Area: ${this._formatArea(area)}`);
      this._stopInput();
    }, Cesium.ScreenSpaceEventType.LEFT_DOUBLE_CLICK);
  }

  /** Start height/elevation measurement — click a single point. */
  startHeight() {
    this.clear();
    this.activeMode = 'height';
    this._setStatus('Click a point to measure elevation');

    this.handler.setInputAction((click) => {
      const ray = this.viewer.camera.getPickRay(click.position);
      const pos = this.viewer.scene.globe.pick(ray, this.viewer.scene);
      if (!Cesium.defined(pos)) return;

      const carto = Cesium.Cartographic.fromCartesian(pos);
      const heightM = carto.height;
      this._addPointEntity(pos);

      // Draw vertical line from surface to the point
      const ground = Cesium.Cartesian3.fromRadians(carto.longitude, carto.latitude, 0);
      this._drawLine(ground, pos, Cesium.Color.CYAN);
      this._addLabel(pos, `Elevation: ${heightM.toFixed(2)} m`);
      this._setStatus(`Elevation: ${heightM.toFixed(2)} m`);
      this._stopInput();
    }, Cesium.ScreenSpaceEventType.LEFT_CLICK);
  }

  /** Remove all measurement entities and reset state. */
  clear() {
    for (const entity of this.entities) {
      this.viewer.entities.remove(entity);
    }
    this.entities = [];
    this.positions = [];
    this.activeMode = null;
    this._stopInput();
    this._setStatus('');
  }

  // ── Internal helpers ──

  _stopInput() {
    this.handler.removeInputAction(Cesium.ScreenSpaceEventType.LEFT_CLICK);
    this.handler.removeInputAction(Cesium.ScreenSpaceEventType.LEFT_DOUBLE_CLICK);
  }

  _addPointEntity(position) {
    const entity = this.viewer.entities.add({
      position,
      point: {
        pixelSize: 8,
        color: Cesium.Color.YELLOW,
        outlineColor: Cesium.Color.BLACK,
        outlineWidth: 1,
        disableDepthTestDistance: Number.POSITIVE_INFINITY,
      },
    });
    this.entities.push(entity);
  }

  _drawLine(a, b, color = Cesium.Color.YELLOW) {
    const entity = this.viewer.entities.add({
      polyline: {
        positions: [a, b],
        width: 2,
        material: color,
        clampToGround: true,
        depthFailMaterial: color.withAlpha(0.4),
      },
    });
    this.entities.push(entity);
  }

  _drawPolygon(positions) {
    const entity = this.viewer.entities.add({
      polygon: {
        hierarchy: new Cesium.PolygonHierarchy(positions),
        material: Cesium.Color.YELLOW.withAlpha(0.2),
        outline: true,
        outlineColor: Cesium.Color.YELLOW,
        perPositionHeight: true,
      },
    });
    this.entities.push(entity);
  }

  _addLabel(position, text) {
    const entity = this.viewer.entities.add({
      position,
      label: {
        text,
        font: '14px sans-serif',
        fillColor: Cesium.Color.WHITE,
        outlineColor: Cesium.Color.BLACK,
        outlineWidth: 2,
        style: Cesium.LabelStyle.FILL_AND_OUTLINE,
        verticalOrigin: Cesium.VerticalOrigin.BOTTOM,
        pixelOffset: new Cesium.Cartesian2(0, -12),
        disableDepthTestDistance: Number.POSITIVE_INFINITY,
        showBackground: true,
        backgroundColor: new Cesium.Color(0, 0, 0, 0.7),
      },
    });
    this.entities.push(entity);
  }

  _computeArea(positions) {
    // Spherical polygon area via Cesium's EllipsoidTangentPlane
    const cartos = positions.map((p) => Cesium.Cartographic.fromCartesian(p));
    // Use the shoelace formula on projected coordinates
    const tangentPlane = Cesium.EllipsoidTangentPlane.fromPoints(
      positions,
      Cesium.Ellipsoid.WGS84
    );
    const projected = tangentPlane.projectPointsOntoPlane(positions);
    let area = 0;
    for (let i = 0; i < projected.length; i++) {
      const j = (i + 1) % projected.length;
      area += projected[i].x * projected[j].y;
      area -= projected[j].x * projected[i].y;
    }
    return Math.abs(area) / 2;
  }

  _computeCentroid(positions) {
    const result = new Cesium.Cartesian3();
    for (const p of positions) {
      Cesium.Cartesian3.add(result, p, result);
    }
    Cesium.Cartesian3.divideByScalar(result, positions.length, result);
    return result;
  }

  _formatDistance(meters) {
    return meters >= 1000 ? `${(meters / 1000).toFixed(3)} km` : `${meters.toFixed(2)} m`;
  }

  _formatArea(sqMeters) {
    if (sqMeters >= 1_000_000) return `${(sqMeters / 1_000_000).toFixed(4)} km²`;
    return `${sqMeters.toFixed(2)} m²`;
  }

  _setStatus(text) {
    const el = document.getElementById('measure-status');
    if (el) el.textContent = text;
  }
}

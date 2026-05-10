/**
 * Story Player — plays narrated 3D presentations with camera transitions.
 *
 * Flies the camera between slides, manages asset visibility,
 * shows annotations, and provides playback controls.
 */

const API = '/api/v1';

export class StoryPlayer {
  constructor(viewer) {
    this.viewer = viewer;
    this.story = null;
    this.currentIndex = 0;
    this.playing = false;
    this.timer = null;
    this.annotationEntities = [];
    this.onSlideChange = null;

    this._createControls();
  }

  load(story) {
    this.story = story;
    this.currentIndex = 0;
    this.playing = false;
    this._clearTimer();
    this._showControls(true);
    this._updateUI();
    if (story.slides.length > 0) {
      this.goToSlide(0);
    }
  }

  play() {
    if (!this.story || this.story.slides.length === 0) return;
    this.playing = true;
    this._updateUI();
    this._scheduleNext();
  }

  pause() {
    this.playing = false;
    this._clearTimer();
    this._updateUI();
  }

  nextSlide() {
    if (!this.story) return;
    if (this.currentIndex < this.story.slides.length - 1) {
      this.goToSlide(this.currentIndex + 1);
    }
  }

  prevSlide() {
    if (!this.story) return;
    if (this.currentIndex > 0) {
      this.goToSlide(this.currentIndex - 1);
    }
  }

  goToSlide(index) {
    if (!this.story || index < 0 || index >= this.story.slides.length) return;

    this.currentIndex = index;
    const slide = this.story.slides[index];

    // Fly camera to slide position
    this._flyToCamera(slide.camera, slide.transition, slide.duration_seconds);

    // Update annotations
    this._showAnnotations(slide.annotations || []);

    // Update progress
    this._updateUI();

    if (this.onSlideChange) {
      this.onSlideChange(index, slide);
    }

    if (this.playing) {
      this._scheduleNext();
    }
  }

  stop() {
    this.playing = false;
    this._clearTimer();
    this._clearAnnotations();
    this._showControls(false);
    this.story = null;
  }

  // ─── Private ─────────────────────────────────────────────────────────

  _flyToCamera(camera, transition, duration) {
    if (!camera) return;

    const dest = Cesium.Cartesian3.fromDegrees(
      camera.longitude, camera.latitude, camera.height
    );

    const heading = Cesium.Math.toRadians(camera.heading || 0);
    const pitch = Cesium.Math.toRadians(camera.pitch || -30);
    const roll = Cesium.Math.toRadians(camera.roll || 0);

    if (transition === 'cut') {
      this.viewer.camera.setView({
        destination: dest,
        orientation: { heading, pitch, roll },
      });
    } else {
      this.viewer.camera.flyTo({
        destination: dest,
        orientation: { heading, pitch, roll },
        duration: transition === 'fade' ? 1.0 : (duration || 3.0),
      });
    }
  }

  _showAnnotations(annotations) {
    this._clearAnnotations();
    for (const ann of annotations) {
      const entity = this.viewer.entities.add({
        position: Cesium.Cartesian3.fromDegrees(ann.longitude, ann.latitude, ann.height || 0),
        label: {
          text: ann.text,
          font: '14px sans-serif',
          fillColor: Cesium.Color.WHITE,
          outlineColor: Cesium.Color.BLACK,
          outlineWidth: 2,
          style: Cesium.LabelStyle.FILL_AND_OUTLINE,
          verticalOrigin: Cesium.VerticalOrigin.BOTTOM,
          pixelOffset: new Cesium.Cartesian2(0, -10),
          disableDepthTestDistance: Number.POSITIVE_INFINITY,
        },
        point: {
          pixelSize: 8,
          color: Cesium.Color.CYAN,
          outlineColor: Cesium.Color.WHITE,
          outlineWidth: 1,
        },
      });
      this.annotationEntities.push(entity);
    }
  }

  _clearAnnotations() {
    for (const entity of this.annotationEntities) {
      this.viewer.entities.remove(entity);
    }
    this.annotationEntities = [];
  }

  _scheduleNext() {
    this._clearTimer();
    if (!this.story || !this.playing) return;

    const slide = this.story.slides[this.currentIndex];
    const delay = (slide.duration_seconds || 5) * 1000;

    this.timer = setTimeout(() => {
      if (this.currentIndex < this.story.slides.length - 1) {
        this.goToSlide(this.currentIndex + 1);
      } else {
        this.pause();
      }
    }, delay);
  }

  _clearTimer() {
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }

  _createControls() {
    const bar = document.createElement('div');
    bar.id = 'story-player-bar';
    bar.className = 'story-player-bar';
    bar.style.display = 'none';
    bar.innerHTML = `
      <button id="sp-prev" title="Previous slide">⏮</button>
      <button id="sp-play" title="Play/Pause">▶</button>
      <button id="sp-next" title="Next slide">⏭</button>
      <span id="sp-progress" class="sp-progress">0 / 0</span>
      <div id="sp-title" class="sp-title"></div>
      <button id="sp-close" title="Stop">✕</button>
    `;
    document.body.appendChild(bar);

    bar.querySelector('#sp-prev').addEventListener('click', () => this.prevSlide());
    bar.querySelector('#sp-play').addEventListener('click', () => {
      if (this.playing) this.pause(); else this.play();
    });
    bar.querySelector('#sp-next').addEventListener('click', () => this.nextSlide());
    bar.querySelector('#sp-close').addEventListener('click', () => this.stop());
  }

  _showControls(visible) {
    const bar = document.getElementById('story-player-bar');
    if (bar) bar.style.display = visible ? 'flex' : 'none';
  }

  _updateUI() {
    if (!this.story) return;
    const progress = document.getElementById('sp-progress');
    const playBtn = document.getElementById('sp-play');
    const title = document.getElementById('sp-title');

    if (progress) {
      progress.textContent = `${this.currentIndex + 1} / ${this.story.slides.length}`;
    }
    if (playBtn) {
      playBtn.textContent = this.playing ? '⏸' : '▶';
    }
    if (title && this.story.slides[this.currentIndex]) {
      title.textContent = this.story.slides[this.currentIndex].title || this.story.title;
    }
  }
}

/** Fetch stories from the API. */
export async function fetchStories() {
  const res = await fetch(`${API}/stories`);
  if (!res.ok) return [];
  return res.json();
}

/** Create a new story. */
export async function createStory(data) {
  const res = await fetch(`${API}/stories`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(data),
  });
  if (!res.ok) throw new Error('Failed to create story');
  return res.json();
}

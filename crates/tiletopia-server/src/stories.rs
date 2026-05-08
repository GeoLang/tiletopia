//! TileTopia Stories — interactive narrated presentations through 3D scenes.
//!
//! Guided tours with waypoints, camera animations, annotations, and embedded media.

use serde::{Deserialize, Serialize};

/// A complete Story (guided tour).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Story {
    pub id: String,
    pub title: String,
    pub description: String,
    pub author_id: String,
    pub slides: Vec<Slide>,
    pub settings: StorySettings,
    pub published: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Story-level settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorySettings {
    pub auto_play: bool,
    pub loop_playback: bool,
    pub default_slide_duration_secs: f64,
    pub transition_type: TransitionType,
    pub background_audio_url: Option<String>,
}

impl Default for StorySettings {
    fn default() -> Self {
        Self {
            auto_play: true,
            loop_playback: false,
            default_slide_duration_secs: 5.0,
            transition_type: TransitionType::Fly,
            background_audio_url: None,
        }
    }
}

/// Camera transition between slides.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum TransitionType {
    Fly,
    Cut,
    Orbit,
    Dolly,
    Custom { duration_secs: f64, easing: String },
}

/// A single slide in a story.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slide {
    pub id: String,
    pub title: Option<String>,
    pub camera: CameraPosition,
    pub duration_secs: Option<f64>,
    pub narration: Option<Narration>,
    pub overlays: Vec<Overlay>,
    pub visible_layers: Vec<String>,
    pub time_of_day: Option<f64>, // 0.0 - 24.0 for lighting
}

/// Camera position and orientation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraPosition {
    pub longitude: f64,
    pub latitude: f64,
    pub height: f64,
    pub heading: f64, // degrees from north
    pub pitch: f64,   // degrees from horizon (-90 to 90)
    pub roll: f64,
}

/// Narration content for a slide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Narration {
    pub text: String,
    pub audio_url: Option<String>,
    pub position: NarrationPosition,
}

/// Where narration text appears on screen.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NarrationPosition {
    Bottom,
    Top,
    Left,
    Right,
    Center,
}

/// Visual overlay on a slide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Overlay {
    pub overlay_type: OverlayType,
    pub position: [f64; 2], // normalized screen position (0-1)
}

/// Types of overlays.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OverlayType {
    Image {
        url: String,
        width: u32,
        height: u32,
    },
    Video {
        url: String,
    },
    Html {
        content: String,
    },
    Annotation {
        text: String,
        style: String,
    },
    Arrow {
        target_lon: f64,
        target_lat: f64,
        target_height: f64,
    },
}

/// Story player state for runtime.
#[derive(Debug, Clone)]
pub struct StoryPlayer {
    pub story_id: String,
    pub current_slide: usize,
    pub playing: bool,
    pub elapsed_secs: f64,
}

impl StoryPlayer {
    pub fn new(story_id: &str) -> Self {
        Self {
            story_id: story_id.to_string(),
            current_slide: 0,
            playing: false,
            elapsed_secs: 0.0,
        }
    }

    pub fn play(&mut self) {
        self.playing = true;
    }

    pub fn pause(&mut self) {
        self.playing = false;
    }

    pub fn next_slide(&mut self, total_slides: usize) -> bool {
        if self.current_slide + 1 < total_slides {
            self.current_slide += 1;
            self.elapsed_secs = 0.0;
            true
        } else {
            false
        }
    }

    pub fn prev_slide(&mut self) -> bool {
        if self.current_slide > 0 {
            self.current_slide -= 1;
            self.elapsed_secs = 0.0;
            true
        } else {
            false
        }
    }

    pub fn go_to_slide(&mut self, index: usize, total_slides: usize) -> bool {
        if index < total_slides {
            self.current_slide = index;
            self.elapsed_secs = 0.0;
            true
        } else {
            false
        }
    }
}

/// Store for managing stories.
pub struct StoryStore {
    stories: std::collections::HashMap<String, Story>,
}

impl StoryStore {
    pub fn new() -> Self {
        Self {
            stories: std::collections::HashMap::new(),
        }
    }

    pub fn create(&mut self, story: Story) -> String {
        let id = story.id.clone();
        self.stories.insert(id.clone(), story);
        id
    }

    pub fn get(&self, id: &str) -> Option<&Story> {
        self.stories.get(id)
    }

    pub fn list_published(&self) -> Vec<&Story> {
        self.stories.values().filter(|s| s.published).collect()
    }

    pub fn list_by_author(&self, author_id: &str) -> Vec<&Story> {
        self.stories
            .values()
            .filter(|s| s.author_id == author_id)
            .collect()
    }

    pub fn delete(&mut self, id: &str) -> bool {
        self.stories.remove(id).is_some()
    }

    /// Generate an embed URL/HTML snippet for sharing.
    pub fn generate_embed_html(story: &Story, base_url: &str) -> String {
        format!(
            r#"<iframe src="{}/stories/{}/embed" width="100%" height="600" frameborder="0" allowfullscreen title="{}"></iframe>"#,
            base_url, story.id, story.title
        )
    }
}

impl Default for StoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_story() -> Story {
        Story {
            id: "story-1".into(),
            title: "Downtown Development Tour".into(),
            description: "Explore the new downtown district".into(),
            author_id: "user-1".into(),
            slides: vec![
                Slide {
                    id: "s1".into(),
                    title: Some("Overview".into()),
                    camera: CameraPosition {
                        longitude: -118.25,
                        latitude: 34.05,
                        height: 500.0,
                        heading: 0.0,
                        pitch: -45.0,
                        roll: 0.0,
                    },
                    duration_secs: Some(5.0),
                    narration: Some(Narration {
                        text: "Welcome to the downtown development project.".into(),
                        audio_url: None,
                        position: NarrationPosition::Bottom,
                    }),
                    overlays: vec![],
                    visible_layers: vec!["buildings".into()],
                    time_of_day: Some(10.0),
                },
                Slide {
                    id: "s2".into(),
                    title: Some("Close-up".into()),
                    camera: CameraPosition {
                        longitude: -118.25,
                        latitude: 34.05,
                        height: 50.0,
                        heading: 90.0,
                        pitch: -15.0,
                        roll: 0.0,
                    },
                    duration_secs: Some(8.0),
                    narration: None,
                    overlays: vec![],
                    visible_layers: vec!["buildings".into(), "terrain".into()],
                    time_of_day: None,
                },
            ],
            settings: StorySettings::default(),
            published: true,
            created_at: "2024-01-01".into(),
            updated_at: "2024-01-15".into(),
        }
    }

    #[test]
    fn test_create_story() {
        let mut store = StoryStore::new();
        store.create(sample_story());
        assert!(store.get("story-1").is_some());
    }

    #[test]
    fn test_story_player_navigation() {
        let story = sample_story();
        let mut player = StoryPlayer::new(&story.id);
        assert_eq!(player.current_slide, 0);
        assert!(player.next_slide(story.slides.len()));
        assert_eq!(player.current_slide, 1);
        assert!(!player.next_slide(story.slides.len())); // Can't go past end
        assert!(player.prev_slide());
        assert_eq!(player.current_slide, 0);
    }

    #[test]
    fn test_embed_html() {
        let story = sample_story();
        let html = StoryStore::generate_embed_html(&story, "https://app.tiletopia.io");
        assert!(html.contains("iframe"));
        assert!(html.contains("story-1"));
        assert!(html.contains("allowfullscreen"));
    }

    #[test]
    fn test_list_published() {
        let mut store = StoryStore::new();
        store.create(sample_story());
        let mut unpublished = sample_story();
        unpublished.id = "story-2".into();
        unpublished.published = false;
        store.create(unpublished);
        assert_eq!(store.list_published().len(), 1);
    }

    #[test]
    fn test_story_settings_default() {
        let settings = StorySettings::default();
        assert!(settings.auto_play);
        assert!(!settings.loop_playback);
        assert_eq!(settings.transition_type, TransitionType::Fly);
    }
}

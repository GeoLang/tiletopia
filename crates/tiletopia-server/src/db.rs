//! Persistent SQLite database for assets, jobs, and API keys.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::users::{Organization, User, UserRole};
use crate::{Asset, AssetStatus, AssetType};

/// Persistent database record for a tiling job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub status: JobStatus,
    pub progress: f64,
    pub input_path: String,
    pub output_format: String,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub points_processed: u64,
    pub tiles_written: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    Done,
    Failed,
}

/// Persistent record for an API key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyRecord {
    pub id: Uuid,
    pub name: String,
    pub token: String,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Persistent record for an annotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnnotationRecord {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub text: String,
    pub longitude: f64,
    pub latitude: f64,
    pub height: f64,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

pub struct Database {
    pub pool: SqlitePool,
}

fn enum_to_str<T: Serialize>(val: &T) -> String {
    serde_json::to_string(val)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
}

fn parse_datetime(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_default()
}

fn parse_optional_datetime(s: Option<String>) -> Option<DateTime<Utc>> {
    s.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    })
}

impl Database {
    pub async fn new(url: &str) -> Result<Self, sqlx::Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS assets (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                asset_type TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                tile_count INTEGER DEFAULT 0,
                size_bytes INTEGER DEFAULT 0,
                description TEXT DEFAULT '',
                tags TEXT DEFAULT '[]'
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS jobs (
                id TEXT PRIMARY KEY,
                asset_id TEXT NOT NULL REFERENCES assets(id),
                status TEXT NOT NULL,
                progress REAL DEFAULT 0.0,
                input_path TEXT NOT NULL,
                output_format TEXT NOT NULL,
                created_at TEXT NOT NULL,
                started_at TEXT,
                completed_at TEXT,
                error TEXT,
                points_processed INTEGER DEFAULT 0,
                tiles_written INTEGER DEFAULT 0
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS api_keys (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                token TEXT NOT NULL UNIQUE,
                scopes TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL,
                expires_at TEXT
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS users (
                id TEXT PRIMARY KEY,
                email TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                password_hash TEXT NOT NULL,
                role TEXT NOT NULL DEFAULT 'viewer',
                org_id TEXT,
                created_at TEXT NOT NULL,
                last_login TEXT
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS organizations (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                created_at TEXT NOT NULL,
                max_storage_bytes INTEGER DEFAULT 10737418240,
                max_assets INTEGER DEFAULT 100
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS annotations (
                id TEXT PRIMARY KEY,
                asset_id TEXT NOT NULL REFERENCES assets(id),
                text TEXT NOT NULL,
                longitude REAL NOT NULL,
                latitude REAL NOT NULL,
                height REAL DEFAULT 0,
                created_at TEXT NOT NULL,
                created_by TEXT
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS stories (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                description TEXT DEFAULT '',
                author_id TEXT,
                slides TEXT NOT NULL DEFAULT '[]',
                is_public INTEGER DEFAULT 0,
                share_token TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS plugins (
                id TEXT PRIMARY KEY,
                manifest TEXT NOT NULL,
                installed_at TEXT NOT NULL,
                enabled INTEGER DEFAULT 1,
                config TEXT DEFAULT '{}'
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS portal_items (
                id TEXT PRIMARY KEY,
                owner_id TEXT NOT NULL,
                title TEXT NOT NULL,
                description TEXT,
                item_type TEXT NOT NULL,
                sharing TEXT NOT NULL DEFAULT 'private',
                config TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // -- Asset CRUD --

    pub async fn create_asset(&self, asset: &Asset) -> Result<(), sqlx::Error> {
        let id = asset.id.to_string();
        let asset_type = enum_to_str(&asset.asset_type);
        let status = enum_to_str(&asset.status);
        let created_at = asset.created_at.to_rfc3339();
        let tags = serde_json::to_string(&asset.tags).unwrap_or_else(|_| "[]".into());

        sqlx::query(
            "INSERT INTO assets (id, name, asset_type, status, created_at, tile_count, size_bytes, description, tags)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&asset.name)
        .bind(&asset_type)
        .bind(&status)
        .bind(&created_at)
        .bind(asset.tile_count as i64)
        .bind(asset.size_bytes as i64)
        .bind(&asset.description)
        .bind(&tags)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_asset(&self, id: Uuid) -> Result<Option<Asset>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, name, asset_type, status, created_at, tile_count, size_bytes, description, tags FROM assets WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_asset(&r)))
    }

    pub async fn list_assets(&self) -> Result<Vec<Asset>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, name, asset_type, status, created_at, tile_count, size_bytes, description, tags FROM assets ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_asset).collect())
    }

    pub async fn update_asset(&self, asset: &Asset) -> Result<(), sqlx::Error> {
        let id = asset.id.to_string();
        let asset_type = enum_to_str(&asset.asset_type);
        let status = enum_to_str(&asset.status);
        let tags = serde_json::to_string(&asset.tags).unwrap_or_else(|_| "[]".into());

        sqlx::query(
            "UPDATE assets SET name = ?, asset_type = ?, status = ?, tile_count = ?, size_bytes = ?, description = ?, tags = ? WHERE id = ?",
        )
        .bind(&asset.name)
        .bind(&asset_type)
        .bind(&status)
        .bind(asset.tile_count as i64)
        .bind(asset.size_bytes as i64)
        .bind(&asset.description)
        .bind(&tags)
        .bind(&id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn delete_asset(&self, id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM assets WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // -- Job CRUD --

    pub async fn create_job(&self, job: &JobRecord) -> Result<(), sqlx::Error> {
        let id = job.id.to_string();
        let asset_id = job.asset_id.to_string();
        let status = enum_to_str(&job.status);
        let created_at = job.created_at.to_rfc3339();
        let started_at = job.started_at.map(|dt| dt.to_rfc3339());
        let completed_at = job.completed_at.map(|dt| dt.to_rfc3339());

        sqlx::query(
            "INSERT INTO jobs (id, asset_id, status, progress, input_path, output_format, created_at, started_at, completed_at, error, points_processed, tiles_written)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&asset_id)
        .bind(&status)
        .bind(job.progress)
        .bind(&job.input_path)
        .bind(&job.output_format)
        .bind(&created_at)
        .bind(&started_at)
        .bind(&completed_at)
        .bind(&job.error)
        .bind(job.points_processed as i64)
        .bind(job.tiles_written as i64)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_job(&self, id: Uuid) -> Result<Option<JobRecord>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, asset_id, status, progress, input_path, output_format, created_at, started_at, completed_at, error, points_processed, tiles_written FROM jobs WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_job(&r)))
    }

    pub async fn list_jobs_for_asset(&self, asset_id: Uuid) -> Result<Vec<JobRecord>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, asset_id, status, progress, input_path, output_format, created_at, started_at, completed_at, error, points_processed, tiles_written FROM jobs WHERE asset_id = ? ORDER BY created_at DESC",
        )
        .bind(asset_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_job).collect())
    }

    pub async fn update_job(&self, job: &JobRecord) -> Result<(), sqlx::Error> {
        let id = job.id.to_string();
        let asset_id = job.asset_id.to_string();
        let status = enum_to_str(&job.status);
        let started_at = job.started_at.map(|dt| dt.to_rfc3339());
        let completed_at = job.completed_at.map(|dt| dt.to_rfc3339());

        sqlx::query(
            "UPDATE jobs SET asset_id = ?, status = ?, progress = ?, input_path = ?, output_format = ?, started_at = ?, completed_at = ?, error = ?, points_processed = ?, tiles_written = ? WHERE id = ?",
        )
        .bind(&asset_id)
        .bind(&status)
        .bind(job.progress)
        .bind(&job.input_path)
        .bind(&job.output_format)
        .bind(&started_at)
        .bind(&completed_at)
        .bind(&job.error)
        .bind(job.points_processed as i64)
        .bind(job.tiles_written as i64)
        .bind(&id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn next_queued_job(&self) -> Result<Option<JobRecord>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, asset_id, status, progress, input_path, output_format, created_at, started_at, completed_at, error, points_processed, tiles_written FROM jobs WHERE status = 'queued' ORDER BY created_at ASC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_job(&r)))
    }

    // -- API Key management --

    pub async fn create_api_key(&self, key: &ApiKeyRecord) -> Result<(), sqlx::Error> {
        let id = key.id.to_string();
        let created_at = key.created_at.to_rfc3339();
        let expires_at = key.expires_at.map(|dt| dt.to_rfc3339());
        let scopes = serde_json::to_string(&key.scopes).unwrap_or_else(|_| "[]".into());

        sqlx::query(
            "INSERT INTO api_keys (id, name, token, scopes, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&key.name)
        .bind(&key.token)
        .bind(&scopes)
        .bind(&created_at)
        .bind(&expires_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_api_key(&self, token: &str) -> Result<Option<ApiKeyRecord>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, name, token, scopes, created_at, expires_at FROM api_keys WHERE token = ?",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_api_key(&r)))
    }

    pub async fn delete_api_key(&self, id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM api_keys WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // -- User management --

    pub async fn create_user(&self, user: &User, password_hash: &str) -> Result<(), sqlx::Error> {
        let id = user.id.to_string();
        let role = enum_to_str(&user.role);
        let created_at = user.created_at.to_rfc3339();
        let last_login = user.last_login.map(|dt| dt.to_rfc3339());
        let org_id = user.org_id.map(|id| id.to_string());

        sqlx::query(
            "INSERT INTO users (id, email, name, password_hash, role, org_id, created_at, last_login)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&user.email)
        .bind(&user.name)
        .bind(password_hash)
        .bind(&role)
        .bind(&org_id)
        .bind(&created_at)
        .bind(&last_login)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_user_by_email(
        &self,
        email: &str,
    ) -> Result<Option<(User, String)>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, email, name, password_hash, role, org_id, created_at, last_login FROM users WHERE email = ?",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| {
            let pw: String = r.get("password_hash");
            (row_to_user(&r), pw)
        }))
    }

    pub async fn get_user(&self, id: Uuid) -> Result<Option<User>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, email, name, password_hash, role, org_id, created_at, last_login FROM users WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_user(&r)))
    }

    pub async fn update_user(&self, user: &User) -> Result<(), sqlx::Error> {
        let id = user.id.to_string();
        let role = enum_to_str(&user.role);
        let last_login = user.last_login.map(|dt| dt.to_rfc3339());
        let org_id = user.org_id.map(|id| id.to_string());

        sqlx::query("UPDATE users SET name = ?, role = ?, org_id = ?, last_login = ? WHERE id = ?")
            .bind(&user.name)
            .bind(&role)
            .bind(&org_id)
            .bind(&last_login)
            .bind(&id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    pub async fn list_users(&self) -> Result<Vec<User>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, email, name, password_hash, role, org_id, created_at, last_login FROM users ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_user).collect())
    }

    pub async fn delete_user(&self, id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // -- Organization management --

    pub async fn create_org(&self, org: &Organization) -> Result<(), sqlx::Error> {
        let id = org.id.to_string();
        let created_at = org.created_at.to_rfc3339();

        sqlx::query(
            "INSERT INTO organizations (id, name, created_at, max_storage_bytes, max_assets)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&org.name)
        .bind(&created_at)
        .bind(org.max_storage_bytes as i64)
        .bind(org.max_assets as i64)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn list_orgs(&self) -> Result<Vec<Organization>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, name, created_at, max_storage_bytes, max_assets FROM organizations ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_org).collect())
    }

    // -- Admin stats --

    pub async fn count_assets(&self) -> Result<u64, sqlx::Error> {
        let row = sqlx::query("SELECT COUNT(*) as cnt FROM assets")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("cnt") as u64)
    }

    pub async fn count_users(&self) -> Result<u64, sqlx::Error> {
        let row = sqlx::query("SELECT COUNT(*) as cnt FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("cnt") as u64)
    }

    pub async fn total_storage_bytes(&self) -> Result<u64, sqlx::Error> {
        let row = sqlx::query("SELECT COALESCE(SUM(size_bytes), 0) as total FROM assets")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("total") as u64)
    }

    pub async fn recent_assets(&self, limit: u32) -> Result<Vec<Asset>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, name, asset_type, status, created_at, tile_count, size_bytes, description, tags FROM assets ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_asset).collect())
    }

    pub async fn list_recent_jobs(&self, limit: u32) -> Result<Vec<JobRecord>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, asset_id, status, progress, input_path, output_format, created_at, started_at, completed_at, error, points_processed, tiles_written FROM jobs ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_job).collect())
    }

    // -- Asset search --

    pub async fn search_assets(
        &self,
        q: Option<&str>,
        tag: Option<&str>,
        asset_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<Asset>, sqlx::Error> {
        let mut sql = String::from(
            "SELECT id, name, asset_type, status, created_at, tile_count, size_bytes, description, tags FROM assets WHERE 1=1",
        );
        let mut binds: Vec<String> = Vec::new();

        if let Some(q) = q {
            sql.push_str(" AND (name LIKE ? OR description LIKE ?)");
            let pattern = format!("%{}%", q);
            binds.push(pattern.clone());
            binds.push(pattern);
        }
        if let Some(tag) = tag {
            sql.push_str(" AND tags LIKE ?");
            binds.push(format!("%\"{}\"%%", tag));
        }
        if let Some(at) = asset_type {
            sql.push_str(" AND asset_type = ?");
            binds.push(at.to_string());
        }
        if let Some(st) = status {
            sql.push_str(" AND status = ?");
            binds.push(st.to_string());
        }
        sql.push_str(" ORDER BY created_at DESC");

        let mut query = sqlx::query(&sql);
        for b in &binds {
            query = query.bind(b);
        }

        let rows = query.fetch_all(&self.pool).await?;
        Ok(rows.iter().map(row_to_asset).collect())
    }

    // -- Annotation CRUD --

    pub async fn create_annotation(&self, ann: &AnnotationRecord) -> Result<(), sqlx::Error> {
        let id = ann.id.to_string();
        let asset_id = ann.asset_id.to_string();
        let created_at = ann.created_at.to_rfc3339();

        sqlx::query(
            "INSERT INTO annotations (id, asset_id, text, longitude, latitude, height, created_at, created_by)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&asset_id)
        .bind(&ann.text)
        .bind(ann.longitude)
        .bind(ann.latitude)
        .bind(ann.height)
        .bind(&created_at)
        .bind(&ann.created_by)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn list_annotations(
        &self,
        asset_id: Uuid,
    ) -> Result<Vec<AnnotationRecord>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, asset_id, text, longitude, latitude, height, created_at, created_by FROM annotations WHERE asset_id = ? ORDER BY created_at DESC",
        )
        .bind(asset_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_annotation).collect())
    }

    pub async fn delete_annotation(&self, annotation_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM annotations WHERE id = ?")
            .bind(annotation_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // -- Story CRUD --

    pub async fn create_story(&self, story: &crate::stories_api::Story) -> Result<(), sqlx::Error> {
        let id = story.id.to_string();
        let slides = serde_json::to_string(&story.slides).unwrap_or_else(|_| "[]".into());
        let author_id = story.author_id.map(|a| a.to_string());
        let created_at = story.created_at.to_rfc3339();
        let updated_at = story.updated_at.to_rfc3339();

        sqlx::query(
            "INSERT INTO stories (id, title, description, author_id, slides, is_public, share_token, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&story.title)
        .bind(&story.description)
        .bind(&author_id)
        .bind(&slides)
        .bind(story.is_public as i32)
        .bind(&story.share_token)
        .bind(&created_at)
        .bind(&updated_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_story(
        &self,
        id: Uuid,
    ) -> Result<Option<crate::stories_api::Story>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, title, description, author_id, slides, is_public, share_token, created_at, updated_at FROM stories WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_story(&r)))
    }

    pub async fn list_stories(&self) -> Result<Vec<crate::stories_api::Story>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, title, description, author_id, slides, is_public, share_token, created_at, updated_at FROM stories ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_story).collect())
    }

    pub async fn update_story(&self, story: &crate::stories_api::Story) -> Result<(), sqlx::Error> {
        let id = story.id.to_string();
        let slides = serde_json::to_string(&story.slides).unwrap_or_else(|_| "[]".into());
        let author_id = story.author_id.map(|a| a.to_string());
        let updated_at = story.updated_at.to_rfc3339();

        sqlx::query(
            "UPDATE stories SET title = ?, description = ?, author_id = ?, slides = ?, is_public = ?, share_token = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&story.title)
        .bind(&story.description)
        .bind(&author_id)
        .bind(&slides)
        .bind(story.is_public as i32)
        .bind(&story.share_token)
        .bind(&updated_at)
        .bind(&id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn delete_story(&self, id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM stories WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_story_by_share_token(
        &self,
        token: &str,
    ) -> Result<Option<crate::stories_api::Story>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, title, description, author_id, slides, is_public, share_token, created_at, updated_at FROM stories WHERE share_token = ? AND is_public = 1",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_story(&r)))
    }

    // -- Plugin Registry CRUD --

    pub async fn install_plugin(
        &self,
        plugin: &crate::plugin_registry::InstalledPlugin,
    ) -> Result<(), sqlx::Error> {
        let manifest = serde_json::to_string(&plugin.manifest).unwrap_or_else(|_| "{}".into());
        let installed_at = plugin.installed_at.to_rfc3339();
        let config = serde_json::to_string(&plugin.config).unwrap_or_else(|_| "{}".into());

        sqlx::query(
            "INSERT INTO plugins (id, manifest, installed_at, enabled, config) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&plugin.manifest.id)
        .bind(&manifest)
        .bind(&installed_at)
        .bind(plugin.enabled as i32)
        .bind(&config)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_plugin(
        &self,
        id: &str,
    ) -> Result<Option<crate::plugin_registry::InstalledPlugin>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, manifest, installed_at, enabled, config FROM plugins WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_plugin(&r)))
    }

    pub async fn list_plugins(
        &self,
    ) -> Result<Vec<crate::plugin_registry::InstalledPlugin>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, manifest, installed_at, enabled, config FROM plugins ORDER BY installed_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_plugin).collect())
    }

    pub async fn update_plugin_config(
        &self,
        id: &str,
        config: &serde_json::Value,
    ) -> Result<(), sqlx::Error> {
        let config_str = serde_json::to_string(config).unwrap_or_else(|_| "{}".into());

        sqlx::query("UPDATE plugins SET config = ? WHERE id = ?")
            .bind(&config_str)
            .bind(id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    pub async fn delete_plugin(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM plugins WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_plugin_enabled(&self, id: &str, enabled: bool) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE plugins SET enabled = ? WHERE id = ?")
            .bind(enabled as i32)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // -- Portal item CRUD --

    pub async fn create_portal_item(
        &self,
        item: &crate::portal::PortalItem,
    ) -> Result<(), sqlx::Error> {
        let id = item.id.to_string();
        let owner_id = item.owner_id.to_string();
        let created_at = item.created.to_rfc3339();
        let updated_at = item.modified.to_rfc3339();
        // display owner + viewer-only fields live in config json
        let config = serde_json::json!({
            "owner": item.owner,
            "tags": item.tags,
            "thumbnail": item.thumbnail,
            "extent": item.extent,
            "metadata": item.metadata,
        });
        let config_str = serde_json::to_string(&config).unwrap_or_else(|_| "{}".into());

        sqlx::query(
            "INSERT INTO portal_items (id, owner_id, title, description, item_type, sharing, config, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&owner_id)
        .bind(&item.title)
        .bind(&item.description)
        .bind(&item.item_type)
        .bind(&item.sharing)
        .bind(&config_str)
        .bind(&created_at)
        .bind(&updated_at)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn list_portal_items_for_viewer(
        &self,
        viewer_id: Uuid,
    ) -> Result<Vec<crate::portal::PortalItem>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, owner_id, title, description, item_type, sharing, config, created_at, updated_at
             FROM portal_items WHERE owner_id = ? OR sharing IN ('public', 'org') ORDER BY updated_at DESC",
        )
        .bind(viewer_id.to_string())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_portal_item).collect())
    }

    pub async fn get_portal_item(
        &self,
        id: Uuid,
    ) -> Result<Option<crate::portal::PortalItem>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT id, owner_id, title, description, item_type, sharing, config, created_at, updated_at
             FROM portal_items WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_portal_item(&r)))
    }

    pub async fn delete_portal_item(&self, id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM portal_items WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn row_to_asset(row: &sqlx::sqlite::SqliteRow) -> Asset {
    let id_str: String = row.get("id");
    let asset_type_str: String = row.get("asset_type");
    let status_str: String = row.get("status");
    let created_at_str: String = row.get("created_at");
    let tags_str: String = row.get("tags");

    Asset {
        id: Uuid::parse_str(&id_str).unwrap_or_default(),
        name: row.get("name"),
        asset_type: serde_json::from_str(&format!("\"{}\"", asset_type_str))
            .unwrap_or(AssetType::PointCloud),
        status: serde_json::from_str(&format!("\"{}\"", status_str))
            .unwrap_or(AssetStatus::Uploading),
        created_at: parse_datetime(&created_at_str),
        tile_count: row.get::<i64, _>("tile_count") as u64,
        size_bytes: row.get::<i64, _>("size_bytes") as u64,
        description: row.get("description"),
        tags: serde_json::from_str(&tags_str).unwrap_or_default(),
    }
}

fn row_to_job(row: &sqlx::sqlite::SqliteRow) -> JobRecord {
    let id_str: String = row.get("id");
    let asset_id_str: String = row.get("asset_id");
    let status_str: String = row.get("status");
    let created_at_str: String = row.get("created_at");
    let started_at: Option<String> = row.get("started_at");
    let completed_at: Option<String> = row.get("completed_at");

    JobRecord {
        id: Uuid::parse_str(&id_str).unwrap_or_default(),
        asset_id: Uuid::parse_str(&asset_id_str).unwrap_or_default(),
        status: serde_json::from_str(&format!("\"{}\"", status_str)).unwrap_or(JobStatus::Queued),
        progress: row.get("progress"),
        input_path: row.get("input_path"),
        output_format: row.get("output_format"),
        created_at: parse_datetime(&created_at_str),
        started_at: parse_optional_datetime(started_at),
        completed_at: parse_optional_datetime(completed_at),
        error: row.get("error"),
        points_processed: row.get::<i64, _>("points_processed") as u64,
        tiles_written: row.get::<i64, _>("tiles_written") as u64,
    }
}

fn row_to_api_key(row: &sqlx::sqlite::SqliteRow) -> ApiKeyRecord {
    let id_str: String = row.get("id");
    let scopes_str: String = row.get("scopes");
    let created_at_str: String = row.get("created_at");
    let expires_at: Option<String> = row.get("expires_at");

    ApiKeyRecord {
        id: Uuid::parse_str(&id_str).unwrap_or_default(),
        name: row.get("name"),
        token: row.get("token"),
        scopes: serde_json::from_str(&scopes_str).unwrap_or_default(),
        created_at: parse_datetime(&created_at_str),
        expires_at: parse_optional_datetime(expires_at),
    }
}

fn row_to_user(row: &sqlx::sqlite::SqliteRow) -> User {
    let id_str: String = row.get("id");
    let role_str: String = row.get("role");
    let created_at_str: String = row.get("created_at");
    let last_login: Option<String> = row.get("last_login");
    let org_id: Option<String> = row.get("org_id");

    User {
        id: Uuid::parse_str(&id_str).unwrap_or_default(),
        email: row.get("email"),
        name: row.get("name"),
        role: serde_json::from_str(&format!("\"{}\"", role_str)).unwrap_or(UserRole::Viewer),
        org_id: org_id.and_then(|s| Uuid::parse_str(&s).ok()),
        created_at: parse_datetime(&created_at_str),
        last_login: parse_optional_datetime(last_login),
    }
}

fn row_to_org(row: &sqlx::sqlite::SqliteRow) -> Organization {
    let id_str: String = row.get("id");
    let created_at_str: String = row.get("created_at");

    Organization {
        id: Uuid::parse_str(&id_str).unwrap_or_default(),
        name: row.get("name"),
        created_at: parse_datetime(&created_at_str),
        max_storage_bytes: row.get::<i64, _>("max_storage_bytes") as u64,
        max_assets: row.get::<i64, _>("max_assets") as u32,
    }
}

fn row_to_annotation(row: &sqlx::sqlite::SqliteRow) -> AnnotationRecord {
    let id_str: String = row.get("id");
    let asset_id_str: String = row.get("asset_id");
    let created_at_str: String = row.get("created_at");

    AnnotationRecord {
        id: Uuid::parse_str(&id_str).unwrap_or_default(),
        asset_id: Uuid::parse_str(&asset_id_str).unwrap_or_default(),
        text: row.get("text"),
        longitude: row.get("longitude"),
        latitude: row.get("latitude"),
        height: row.get("height"),
        created_at: parse_datetime(&created_at_str),
        created_by: row.get("created_by"),
    }
}

fn row_to_story(row: &sqlx::sqlite::SqliteRow) -> crate::stories_api::Story {
    let id_str: String = row.get("id");
    let author_id: Option<String> = row.get("author_id");
    let slides_str: String = row.get("slides");
    let created_at_str: String = row.get("created_at");
    let updated_at_str: String = row.get("updated_at");
    let is_public: i32 = row.get("is_public");

    crate::stories_api::Story {
        id: Uuid::parse_str(&id_str).unwrap_or_default(),
        title: row.get("title"),
        description: row.get("description"),
        author_id: author_id.and_then(|s| Uuid::parse_str(&s).ok()),
        slides: serde_json::from_str(&slides_str).unwrap_or_default(),
        is_public: is_public != 0,
        share_token: row.get("share_token"),
        created_at: parse_datetime(&created_at_str),
        updated_at: parse_datetime(&updated_at_str),
    }
}

fn row_to_portal_item(row: &sqlx::sqlite::SqliteRow) -> crate::portal::PortalItem {
    let id_str: String = row.get("id");
    let owner_id_str: String = row.get("owner_id");
    let config_str: String = row.get("config");
    let created_at_str: String = row.get("created_at");
    let updated_at_str: String = row.get("updated_at");
    let config: serde_json::Value = serde_json::from_str(&config_str).unwrap_or_default();

    crate::portal::PortalItem {
        id: Uuid::parse_str(&id_str).unwrap_or_default(),
        owner_id: Uuid::parse_str(&owner_id_str).unwrap_or_default(),
        title: row.get("title"),
        item_type: row.get("item_type"),
        owner: config
            .get("owner")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        description: row.get("description"),
        tags: config_field(&config, "tags"),
        sharing: row.get("sharing"),
        thumbnail: config_field(&config, "thumbnail"),
        created: parse_datetime(&created_at_str),
        modified: parse_datetime(&updated_at_str),
        extent: config_field(&config, "extent"),
        metadata: config.get("metadata").filter(|v| !v.is_null()).cloned(),
    }
}

fn config_field<T: serde::de::DeserializeOwned>(
    config: &serde_json::Value,
    key: &str,
) -> Option<T> {
    config
        .get(key)
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
}

fn row_to_plugin(row: &sqlx::sqlite::SqliteRow) -> crate::plugin_registry::InstalledPlugin {
    let manifest_str: String = row.get("manifest");
    let installed_at_str: String = row.get("installed_at");
    let enabled: i32 = row.get("enabled");
    let config_str: String = row.get("config");

    crate::plugin_registry::InstalledPlugin {
        manifest: serde_json::from_str(&manifest_str).unwrap_or_else(|_| {
            crate::plugin_registry::PluginManifest {
                id: row.get("id"),
                name: String::new(),
                version: String::new(),
                description: String::new(),
                author: String::new(),
                license: String::new(),
                entry_point: String::new(),
                capabilities: Vec::new(),
                config_schema: None,
            }
        }),
        installed_at: parse_datetime(&installed_at_str),
        enabled: enabled != 0,
        config: serde_json::from_str(&config_str).unwrap_or_default(),
    }
}

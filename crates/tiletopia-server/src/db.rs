//! Persistent SQLite database for assets, jobs, and API keys.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::api_keys::{ApiKey, Permission, RateLimitTier};
use crate::users::{Organization, User, UserRole};
use crate::{Asset, AssetStatus, AssetType};

/// Where a model with local coordinates sits on the globe, and which CRS its
/// coordinates are in. The uploader supplies these, the external tiler has its
/// own defaults for whatever is left out.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelPlacement {
    pub longitude: Option<f64>,
    pub latitude: Option<f64>,
    pub crs: Option<String>,
}

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
    #[serde(default)]
    pub placement: ModelPlacement,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    Done,
    Failed,
}

/// Registry row for one vector tileset: the PMTiles archive tippecanoe built,
/// the martin source serving it, and the run that produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TilesetRecord {
    pub id: Uuid,
    pub name: String,
    pub status: TilesetStatus,
    /// The `/martin/{source}` id the archive is registered under.
    pub source_id: String,
    /// The archive's key inside the tileset directory.
    pub object_key: String,
    pub original_filename: String,
    /// The layer name inside the archive, passed to tippecanoe as `-l`.
    pub layer_name: String,
    /// The tippecanoe argv this archive was built with.
    pub argv: Vec<String>,
    /// The built archive's size. 0 until the build succeeds.
    pub size_bytes: u64,
    pub created_at: DateTime<Utc>,
    pub built_at: Option<DateTime<Utc>>,
    /// The tail of tippecanoe's stderr when the build failed.
    pub error: Option<String>,
    /// JWT `sub` of the uploader. Never serialized: it is an authz field and
    /// would leak user ids to every reader of the tileset list.
    #[serde(skip_serializing)]
    pub owner_id: String,
    /// When the builder claimed this row. Set means a build already started,
    /// which is what keeps the worker from picking one row up twice.
    #[serde(skip_serializing)]
    pub started_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TilesetStatus {
    Building,
    Ready,
    Failed,
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

const ASSET_COLUMNS: &str =
    "id, name, asset_type, status, created_at, tile_count, size_bytes, description, tags, owner_id";

const JOB_COLUMNS: &str = "id, asset_id, status, progress, input_path, output_format, created_at, started_at, completed_at, error, points_processed, tiles_written, longitude, latitude, crs";

const TILESET_COLUMNS: &str = "id, name, status, source_id, object_key, original_filename, layer_name, argv, size_bytes, created_at, started_at, built_at, error, owner_id";

const API_KEY_COLUMNS: &str = "id, name, key_hash, permissions, tier, created_by, created_at, expires_at, last_used_at, revoked";

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
                ion_id INTEGER,
                name TEXT NOT NULL,
                asset_type TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                tile_count INTEGER DEFAULT 0,
                size_bytes INTEGER DEFAULT 0,
                description TEXT DEFAULT '',
                tags TEXT DEFAULT '[]',
                owner_id TEXT
            )",
        )
        .execute(&self.pool)
        .await?;

        // assets predates owner_id, so existing databases need it added. sqlite
        // has no ADD COLUMN IF NOT EXISTS; rows created before this stay NULL.
        let has_owner_id =
            sqlx::query("SELECT 1 FROM pragma_table_info('assets') WHERE name = 'owner_id'")
                .fetch_optional(&self.pool)
                .await?
                .is_some();
        if !has_owner_id {
            sqlx::query("ALTER TABLE assets ADD COLUMN owner_id TEXT")
                .execute(&self.pool)
                .await?;
        }

        // assets predates ion_id. sqlite cannot add a column with a UNIQUE
        // constraint, so the index below is what keeps two assets off one number
        let has_ion_id =
            sqlx::query("SELECT 1 FROM pragma_table_info('assets') WHERE name = 'ion_id'")
                .fetch_optional(&self.pool)
                .await?
                .is_some();
        if !has_ion_id {
            sqlx::query("ALTER TABLE assets ADD COLUMN ion_id INTEGER")
                .execute(&self.pool)
                .await?;
        }
        self.number_assets_without_an_ion_id().await?;
        sqlx::query("CREATE UNIQUE INDEX IF NOT EXISTS assets_ion_id ON assets(ion_id)")
            .execute(&self.pool)
            .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS ion_id_counter (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                last_ion_id INTEGER NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;

        // the counter only ever climbs, so deleting an asset does not put its
        // number back in circulation for a client holding a stale link
        sqlx::query(
            "INSERT INTO ion_id_counter (id, last_ion_id)
             VALUES (1, (SELECT COALESCE(MAX(ion_id), 0) FROM assets))
             ON CONFLICT(id) DO UPDATE SET last_ion_id = MAX(last_ion_id, excluded.last_ion_id)",
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
                tiles_written INTEGER DEFAULT 0,
                longitude REAL,
                latitude REAL,
                crs TEXT
            )",
        )
        .execute(&self.pool)
        .await?;

        // jobs predates the placement the external tiler takes. same
        // ADD COLUMN dance as assets above; rows written before this stay NULL,
        // which is what "the uploader gave no placement" already means
        for column in ["longitude REAL", "latitude REAL", "crs TEXT"] {
            let name = column.split(' ').next().unwrap_or_default();
            let present = sqlx::query("SELECT 1 FROM pragma_table_info('jobs') WHERE name = ?")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?
                .is_some();
            if !present {
                sqlx::query(&format!("ALTER TABLE jobs ADD COLUMN {column}"))
                    .execute(&self.pool)
                    .await?;
            }
        }

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS tilesets (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                status TEXT NOT NULL,
                source_id TEXT NOT NULL UNIQUE,
                object_key TEXT NOT NULL,
                original_filename TEXT NOT NULL,
                layer_name TEXT NOT NULL,
                argv TEXT NOT NULL DEFAULT '[]',
                size_bytes INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                started_at TEXT,
                built_at TEXT,
                error TEXT,
                owner_id TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;

        // api_keys used to hold a plaintext `token` column and had no writer, so
        // no row was ever stored in that shape. Dropping it is what lets the
        // hashed shape below be created on a database that saw the old one.
        let stores_plaintext_tokens =
            sqlx::query("SELECT 1 FROM pragma_table_info('api_keys') WHERE name = 'token'")
                .fetch_optional(&self.pool)
                .await?
                .is_some();
        if stores_plaintext_tokens {
            sqlx::query("DROP TABLE api_keys")
                .execute(&self.pool)
                .await?;
        }

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS api_keys (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                key_hash TEXT NOT NULL UNIQUE,
                permissions TEXT NOT NULL DEFAULT '[]',
                tier TEXT NOT NULL,
                created_by TEXT NOT NULL,
                created_at TEXT NOT NULL,
                expires_at TEXT,
                last_used_at TEXT,
                revoked INTEGER NOT NULL DEFAULT 0
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

    /// Gives an ion id to every asset row that has none, so a database written
    /// before ion ids existed answers the Ion API the same as a fresh one.
    async fn number_assets_without_an_ion_id(&self) -> Result<(), sqlx::Error> {
        let unnumbered: Vec<String> =
            sqlx::query_scalar("SELECT id FROM assets WHERE ion_id IS NULL ORDER BY created_at")
                .fetch_all(&self.pool)
                .await?;

        let mut next: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(ion_id), 0) FROM assets")
            .fetch_one(&self.pool)
            .await?;

        for id in unnumbered {
            next += 1;
            sqlx::query("UPDATE assets SET ion_id = ? WHERE id = ?")
                .bind(next)
                .bind(id)
                .execute(&self.pool)
                .await?;
        }

        Ok(())
    }

    // -- Asset CRUD --

    /// Returns the asset's ion id. Ion asset ids are numbers, so every asset
    /// gets one at creation: a client that read an id off the Ion asset list
    /// has nothing else to ask for the asset by.
    pub async fn create_asset(&self, asset: &Asset) -> Result<i64, sqlx::Error> {
        let id = asset.id.to_string();
        let asset_type = enum_to_str(&asset.asset_type);
        let status = enum_to_str(&asset.status);
        let created_at = asset.created_at.to_rfc3339();
        let tags = serde_json::to_string(&asset.tags).unwrap_or_else(|_| "[]".into());

        // one statement, so two creates at once take different numbers, and the
        // unique index on ion_id refuses a repeat rather than leaving two assets
        // a client cannot tell apart
        let ion_id: i64 = sqlx::query_scalar(
            "UPDATE ion_id_counter SET last_ion_id = last_ion_id + 1 WHERE id = 1 RETURNING last_ion_id",
        )
        .fetch_one(&self.pool)
        .await?;

        sqlx::query(
            "INSERT INTO assets (id, ion_id, name, asset_type, status, created_at, tile_count, size_bytes, description, tags, owner_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(ion_id)
        .bind(&asset.name)
        .bind(&asset_type)
        .bind(&status)
        .bind(&created_at)
        .bind(asset.tile_count as i64)
        .bind(asset.size_bytes as i64)
        .bind(&asset.description)
        .bind(&tags)
        .bind(&asset.owner_id)
        .execute(&self.pool)
        .await?;

        Ok(ion_id)
    }

    pub async fn get_asset(&self, id: Uuid) -> Result<Option<Asset>, sqlx::Error> {
        let row = sqlx::query(&format!("SELECT {ASSET_COLUMNS} FROM assets WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.map(|r| row_to_asset(&r)))
    }

    pub async fn get_asset_with_ion_id(
        &self,
        id: Uuid,
    ) -> Result<Option<(Asset, i64)>, sqlx::Error> {
        let row = sqlx::query(&format!(
            "SELECT {ASSET_COLUMNS}, ion_id FROM assets WHERE id = ?"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| (row_to_asset(&r), r.get("ion_id"))))
    }

    pub async fn get_asset_by_ion_id(
        &self,
        ion_id: i64,
    ) -> Result<Option<(Asset, i64)>, sqlx::Error> {
        let row = sqlx::query(&format!(
            "SELECT {ASSET_COLUMNS}, ion_id FROM assets WHERE ion_id = ?"
        ))
        .bind(ion_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| (row_to_asset(&r), r.get("ion_id"))))
    }

    pub async fn list_assets(&self) -> Result<Vec<Asset>, sqlx::Error> {
        let rows = sqlx::query(&format!(
            "SELECT {ASSET_COLUMNS} FROM assets ORDER BY created_at DESC"
        ))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_asset).collect())
    }

    pub async fn list_assets_with_ion_ids(&self) -> Result<Vec<(Asset, i64)>, sqlx::Error> {
        let rows = sqlx::query(&format!(
            "SELECT {ASSET_COLUMNS}, ion_id FROM assets ORDER BY created_at DESC"
        ))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| (row_to_asset(r), r.get("ion_id")))
            .collect())
    }

    pub async fn update_asset(&self, asset: &Asset) -> Result<(), sqlx::Error> {
        let id = asset.id.to_string();
        let asset_type = enum_to_str(&asset.asset_type);
        let status = enum_to_str(&asset.status);
        let tags = serde_json::to_string(&asset.tags).unwrap_or_else(|_| "[]".into());

        // owner_id is set once at create and deliberately left out here, so an
        // update carrying a stale Asset can never reassign or clear ownership
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
        // jobs reference assets without a cascade; remove them first
        sqlx::query("DELETE FROM jobs WHERE asset_id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
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

        sqlx::query(&format!(
            "INSERT INTO jobs ({JOB_COLUMNS})
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
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
        .bind(job.placement.longitude)
        .bind(job.placement.latitude)
        .bind(&job.placement.crs)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_job(&self, id: Uuid) -> Result<Option<JobRecord>, sqlx::Error> {
        let row = sqlx::query(&format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?"))
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.map(|r| row_to_job(&r)))
    }

    pub async fn list_jobs_for_asset(&self, asset_id: Uuid) -> Result<Vec<JobRecord>, sqlx::Error> {
        let rows = sqlx::query(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs WHERE asset_id = ? ORDER BY created_at DESC"
        ))
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
        let row = sqlx::query(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs WHERE status = 'queued' ORDER BY created_at ASC LIMIT 1"
        ))
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_job(&r)))
    }

    // -- Tileset registry --

    pub async fn create_tileset(&self, tileset: &TilesetRecord) -> Result<(), sqlx::Error> {
        sqlx::query(&format!(
            "INSERT INTO tilesets ({TILESET_COLUMNS})
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(tileset.id.to_string())
        .bind(&tileset.name)
        .bind(enum_to_str(&tileset.status))
        .bind(&tileset.source_id)
        .bind(&tileset.object_key)
        .bind(&tileset.original_filename)
        .bind(&tileset.layer_name)
        .bind(serde_json::to_string(&tileset.argv).unwrap_or_else(|_| "[]".into()))
        .bind(tileset.size_bytes as i64)
        .bind(tileset.created_at.to_rfc3339())
        .bind(tileset.started_at.map(|dt| dt.to_rfc3339()))
        .bind(tileset.built_at.map(|dt| dt.to_rfc3339()))
        .bind(&tileset.error)
        .bind(&tileset.owner_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    pub async fn get_tileset(&self, id: Uuid) -> Result<Option<TilesetRecord>, sqlx::Error> {
        let row = sqlx::query(&format!(
            "SELECT {TILESET_COLUMNS} FROM tilesets WHERE id = ?"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_tileset(&r)))
    }

    pub async fn list_tilesets(&self) -> Result<Vec<TilesetRecord>, sqlx::Error> {
        let rows = sqlx::query(&format!(
            "SELECT {TILESET_COLUMNS} FROM tilesets ORDER BY created_at DESC"
        ))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_tileset).collect())
    }

    /// Every archive that finished building, which is what a restart
    /// re-registers with the martin backend.
    pub async fn list_ready_tilesets(&self) -> Result<Vec<TilesetRecord>, sqlx::Error> {
        let rows = sqlx::query(&format!(
            "SELECT {TILESET_COLUMNS} FROM tilesets WHERE status = 'ready' ORDER BY created_at"
        ))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_tileset).collect())
    }

    /// Mark the oldest unclaimed build as started and hand it back. The claim
    /// and the read are one statement, so two workers cannot take one row.
    pub async fn claim_tileset_build(&self) -> Result<Option<TilesetRecord>, sqlx::Error> {
        let row = sqlx::query(&format!(
            "UPDATE tilesets SET started_at = ?
             WHERE id = (
                 SELECT id FROM tilesets
                 WHERE status = 'building' AND started_at IS NULL
                 ORDER BY created_at LIMIT 1
             )
             RETURNING {TILESET_COLUMNS}"
        ))
        .bind(Utc::now().to_rfc3339())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_tileset(&r)))
    }

    /// Put every build that was running when the server stopped back in the
    /// queue. The uploaded input is still on disk until a build reaches a
    /// terminal state, so the retry has something to read.
    pub async fn requeue_claimed_tileset_builds(&self) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE tilesets SET started_at = NULL WHERE status = 'building' AND started_at IS NOT NULL",
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    /// Write back the build's outcome, reporting whether the row was still
    /// there. Identity and build parameters are set once at creation and left
    /// out here, so they cannot drift from the archive the row names.
    pub async fn finish_tileset(&self, tileset: &TilesetRecord) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE tilesets SET status = ?, size_bytes = ?, built_at = ?, error = ? WHERE id = ?",
        )
        .bind(enum_to_str(&tileset.status))
        .bind(tileset.size_bytes as i64)
        .bind(tileset.built_at.map(|dt| dt.to_rfc3339()))
        .bind(&tileset.error)
        .bind(tileset.id.to_string())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    pub async fn delete_tileset(&self, id: Uuid) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM tilesets WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected())
    }

    // -- API Key management --

    pub async fn create_api_key(&self, key: &ApiKey) -> Result<(), sqlx::Error> {
        let permissions = serde_json::to_string(&key.permissions).unwrap_or_else(|_| "[]".into());

        sqlx::query(
            "INSERT INTO api_keys \
             (id, name, key_hash, permissions, tier, created_by, created_at, expires_at, last_used_at, revoked) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(key.id.to_string())
        .bind(&key.name)
        .bind(&key.key_hash)
        .bind(&permissions)
        .bind(key.tier.name())
        .bind(&key.created_by)
        .bind(key.created_at.to_rfc3339())
        .bind(key.expires_at.map(|dt| dt.to_rfc3339()))
        .bind(key.last_used_at.map(|dt| dt.to_rfc3339()))
        .bind(key.revoked as i64)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// The key with this digest. Exact match on the UNIQUE `key_hash` column, so
    /// resolving a credential is one indexed lookup rather than a scan.
    ///
    /// Revoked and expired rows come back like any other: the credential path
    /// decides, so it can answer which of the two it was.
    pub async fn api_key_by_hash(&self, key_hash: &str) -> Result<Option<ApiKey>, sqlx::Error> {
        let row = sqlx::query(&format!(
            "SELECT {API_KEY_COLUMNS} FROM api_keys WHERE key_hash = ?"
        ))
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| row_to_api_key(&r)))
    }

    pub async fn list_api_keys(&self) -> Result<Vec<ApiKey>, sqlx::Error> {
        let rows = sqlx::query(&format!(
            "SELECT {API_KEY_COLUMNS} FROM api_keys ORDER BY created_at DESC"
        ))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_api_key).collect())
    }

    /// Mark a key dead, keeping the row so a leaked key cannot come back and the
    /// listing still shows it. Returns the rows changed.
    pub async fn revoke_api_key(&self, id: Uuid) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("UPDATE api_keys SET revoked = 1 WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn delete_api_key(&self, id: Uuid) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM api_keys WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn touch_api_key(&self, id: Uuid, at: DateTime<Utc>) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE api_keys SET last_used_at = ? WHERE id = ?")
            .bind(at.to_rfc3339())
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

    pub async fn set_password_hash(
        &self,
        id: Uuid,
        password_hash: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
            .bind(password_hash)
            .bind(id.to_string())
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
            "SELECT id, name, asset_type, status, created_at, tile_count, size_bytes, description, tags, owner_id FROM assets ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_asset).collect())
    }

    pub async fn list_recent_jobs(&self, limit: u32) -> Result<Vec<JobRecord>, sqlx::Error> {
        let rows = sqlx::query(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs ORDER BY created_at DESC LIMIT ?"
        ))
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
        let mut sql = format!("SELECT {ASSET_COLUMNS} FROM assets WHERE 1=1");
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

    /// Delete an annotation belonging to `asset_id`. Returns how many rows went,
    /// so a caller that authorized against one asset cannot delete an annotation
    /// hanging off another. Zero means no such annotation on that asset.
    pub async fn delete_annotation(
        &self,
        asset_id: Uuid,
        annotation_id: Uuid,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM annotations WHERE id = ? AND asset_id = ?")
            .bind(annotation_id.to_string())
            .bind(asset_id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
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
        owner_id: row.get("owner_id"),
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
        placement: ModelPlacement {
            longitude: row.get("longitude"),
            latitude: row.get("latitude"),
            crs: row.get("crs"),
        },
    }
}

/// A stored key. An unreadable permission or tier maps to the narrowest thing
/// there is, so a hand-edited row can only ever lose reach.
fn row_to_api_key(row: &sqlx::sqlite::SqliteRow) -> ApiKey {
    let id_str: String = row.get("id");
    let permissions_str: String = row.get("permissions");
    let tier_str: String = row.get("tier");
    let created_at_str: String = row.get("created_at");
    let expires_at: Option<String> = row.get("expires_at");
    let last_used_at: Option<String> = row.get("last_used_at");

    let permission_names: Vec<String> = serde_json::from_str(&permissions_str).unwrap_or_default();

    ApiKey {
        id: Uuid::parse_str(&id_str).unwrap_or_default(),
        name: row.get("name"),
        key_hash: row.get("key_hash"),
        permissions: permission_names
            .iter()
            .filter_map(|name| Permission::from_name(name))
            .collect(),
        tier: RateLimitTier::from_name(&tier_str).unwrap_or(RateLimitTier::Free),
        created_by: row.get("created_by"),
        created_at: parse_datetime(&created_at_str),
        last_used_at: parse_optional_datetime(last_used_at),
        expires_at: parse_optional_datetime(expires_at),
        revoked: row.get::<i64, _>("revoked") != 0,
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

fn row_to_tileset(row: &sqlx::sqlite::SqliteRow) -> TilesetRecord {
    let id_str: String = row.get("id");
    let status_str: String = row.get("status");
    let argv_str: String = row.get("argv");
    let created_at_str: String = row.get("created_at");

    TilesetRecord {
        id: Uuid::parse_str(&id_str).unwrap_or_default(),
        name: row.get("name"),
        status: serde_json::from_str(&format!("\"{status_str}\"")).unwrap_or(TilesetStatus::Failed),
        source_id: row.get("source_id"),
        object_key: row.get("object_key"),
        original_filename: row.get("original_filename"),
        layer_name: row.get("layer_name"),
        argv: serde_json::from_str(&argv_str).unwrap_or_default(),
        size_bytes: row.get::<i64, _>("size_bytes") as u64,
        created_at: parse_datetime(&created_at_str),
        started_at: parse_optional_datetime(row.get("started_at")),
        built_at: parse_optional_datetime(row.get("built_at")),
        error: row.get("error"),
        owner_id: row.get("owner_id"),
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

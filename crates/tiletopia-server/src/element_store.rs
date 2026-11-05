use crate::element_query::{ElementFilter, GroupBy};
use crate::users::to_hex;
use futures::TryStreamExt;
use futures::stream::BoxStream;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteRow, SqliteSynchronous};
use sqlx::{ConnectOptions, Connection, QueryBuilder, Row, Sqlite, SqliteConnection};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tiletopia_core::mesh_tiler::MeshData;
use tiletopia_ingest::IngestError;
use tiletopia_ingest::ifc_elements::{
    ElementLocation, ElementProperty, ExtractedElement, ExtractionWarnings, ModelOrigin,
    PropertyValue, element_locations, extract_elements,
};
use tiletopia_ingest::ifc_reader::MapConversionPlacement;

const STORE_FILE_NAME: &str = "elements.sqlite";
const BUILDING_FILE_NAME: &str = "elements.sqlite.building";

const ELEMENTS_PER_FLUSH: usize = 256;
const PROPERTY_ROWS_PER_FLUSH: usize = 4096;
const PROPERTY_ROWS_PER_STATEMENT: usize = 1024;
const TEXT_BYTES_PER_FLUSH: usize = 16 * 1024 * 1024;

const QUERY_TIME_LIMIT: Duration = Duration::from_secs(10);
const VIRTUAL_MACHINE_STEPS_BETWEEN_CLOCK_READS: i32 = 10_000;

pub const MAX_GROUPS: usize = 1000;
pub const MAX_TREE_CHILDREN: usize = 50_000;
const SEARCH_PAGE_SIZE: i64 = 50;
const MAX_TREE_DEPTH: i64 = 64;

const SCHEMA: &[&str] = &[
    "CREATE TABLE elements (
        element_row INTEGER PRIMARY KEY,
        express_id INTEGER NOT NULL,
        global_id TEXT NOT NULL,
        class TEXT NOT NULL,
        name TEXT,
        tree_kind TEXT NOT NULL,
        parent_express_id INTEGER,
        storey TEXT,
        longitude REAL,
        latitude REAL,
        height REAL,
        radius REAL,
        mesh_hash TEXT,
        base_height REAL
    )",
    "CREATE TABLE properties (
        element_row INTEGER NOT NULL,
        key TEXT NOT NULL,
        value_text TEXT,
        value_number REAL,
        value_boolean INTEGER,
        units TEXT
    )",
    "CREATE VIRTUAL TABLE element_search USING fts5(
        global_id, name, class, property_values,
        tokenize = 'unicode61 remove_diacritics 2'
    )",
    "CREATE TABLE warnings (name TEXT PRIMARY KEY, count INTEGER NOT NULL)",
    "CREATE TABLE build (requested_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
];

const INDEXES: &[&str] = &[
    "CREATE INDEX elements_by_global_id ON elements (global_id)",
    "CREATE INDEX elements_by_express_id ON elements (express_id)",
    "CREATE INDEX elements_by_parent ON elements (parent_express_id)",
    "CREATE INDEX elements_by_class ON elements (class)",
    "CREATE INDEX elements_by_storey ON elements (storey)",
    "CREATE INDEX properties_by_element ON properties (element_row, key)",
    "CREATE INDEX properties_by_number ON properties (key, value_number, element_row)",
    "CREATE INDEX properties_by_text ON properties (key, value_text, element_row)",
];

pub fn store_path(asset_dir: &Path) -> PathBuf {
    asset_dir.join(STORE_FILE_NAME)
}

fn remove_file_if_present(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not remove {}: {error}", path.display())),
    }
}

pub fn remove_store(asset_dir: &Path) -> Result<(), String> {
    remove_file_if_present(&store_path(asset_dir))?;
    remove_file_if_present(&asset_dir.join(BUILDING_FILE_NAME))
}

fn word_parts(word: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut previous: Option<char> = None;
    for character in word.chars() {
        let starts_part = previous.is_some_and(|previous| {
            (previous.is_lowercase() && character.is_uppercase())
                || (previous.is_alphabetic() && character.is_numeric())
                || (previous.is_numeric() && character.is_alphabetic())
        });
        if starts_part {
            parts.push(std::mem::take(&mut part));
        }
        part.push(character);
        previous = Some(character);
    }
    parts.push(part);
    parts
}

fn with_word_parts(text: &str) -> String {
    let mut indexed = text.to_string();
    for word in text.split(|character: char| !character.is_alphanumeric()) {
        let parts = word_parts(word);
        if parts.len() > 1 {
            indexed.push(' ');
            indexed.push_str(&parts.join(" "));
        }
    }
    indexed
}

fn indexed_property_values(properties: &[ElementProperty]) -> String {
    let mut values = Vec::with_capacity(properties.len());
    for property in properties {
        match &property.value {
            PropertyValue::Text(text) => values.push(with_word_parts(text)),
            PropertyValue::Number(number) => values.push(number.to_string()),
            PropertyValue::Boolean(_) => {}
        }
    }
    values.join(" ")
}

// mesh coordinates are metres, and two exports of one model differ below a millimetre
const MESH_HASH_UNITS_PER_METRE: f64 = 1000.0;

fn element_mesh_hashes(meshes: &[MeshData]) -> HashMap<String, String> {
    let mut digests_by_element: HashMap<&str, Vec<Vec<u8>>> = HashMap::new();
    for mesh in meshes {
        let Some(element_id) = mesh.asset_id.as_deref() else {
            continue;
        };
        let mut digest = Sha256::new();
        for index in &mesh.indices {
            for coordinate in mesh.positions[*index as usize] {
                let rounded = (coordinate * MESH_HASH_UNITS_PER_METRE).round() as i64;
                digest.update(rounded.to_le_bytes());
            }
        }
        digests_by_element
            .entry(element_id)
            .or_default()
            .push(digest.finalize().to_vec());
    }
    digests_by_element
        .into_iter()
        .map(|(element_id, mut digests)| {
            digests.sort_unstable();
            let mut combined = Sha256::new();
            for digest in &digests {
                combined.update(digest);
            }
            (element_id.to_string(), to_hex(&combined.finalize()))
        })
        .collect()
}

struct StoreWriter {
    connection: SqliteConnection,
    mesh_hashes: HashMap<String, String>,
    pending: Vec<ExtractedElement>,
    pending_property_rows: usize,
    pending_text_bytes: usize,
    next_row: i64,
}

impl StoreWriter {
    async fn create(
        path: &Path,
        mesh_hashes: HashMap<String, String>,
    ) -> Result<Self, sqlx::Error> {
        // the build file is thrown away on any failure, so it needs no journal
        let mut connection = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Off)
            .synchronous(SqliteSynchronous::Off)
            .connect()
            .await?;
        for statement in SCHEMA {
            sqlx::query(*statement).execute(&mut connection).await?;
        }
        sqlx::query("BEGIN").execute(&mut connection).await?;
        Ok(Self {
            connection,
            mesh_hashes,
            pending: Vec::with_capacity(ELEMENTS_PER_FLUSH),
            pending_property_rows: 0,
            pending_text_bytes: 0,
            next_row: 1,
        })
    }

    fn is_full(&self) -> bool {
        self.pending.len() >= ELEMENTS_PER_FLUSH
            || self.pending_property_rows >= PROPERTY_ROWS_PER_FLUSH
            || self.pending_text_bytes >= TEXT_BYTES_PER_FLUSH
    }

    fn push(&mut self, element: ExtractedElement) {
        self.pending_property_rows += element.properties.len();
        self.pending_text_bytes += element.text_bytes();
        self.pending.push(element);
    }

    async fn insert_elements(
        &mut self,
        rows: &[(i64, &ExtractedElement)],
    ) -> Result<(), sqlx::Error> {
        let mut query = QueryBuilder::<Sqlite>::new(
            "INSERT INTO elements (element_row, express_id, global_id, class, name, tree_kind, \
             parent_express_id, storey, longitude, latitude, height, radius, mesh_hash, base_height) ",
        );
        query.push_values(rows, |mut row, (element_row, element)| {
            let location: Option<ElementLocation> = element.location;
            row.push_bind(*element_row)
                .push_bind(i64::from(element.express_id))
                .push_bind(element.global_id.clone())
                .push_bind(element.class.clone())
                .push_bind(element.name.clone())
                .push_bind(element.tree_kind.as_str().to_string())
                .push_bind(element.parent_express_id.map(i64::from))
                .push_bind(element.storey.clone())
                .push_bind(location.map(|location| location.longitude))
                .push_bind(location.map(|location| location.latitude))
                .push_bind(location.map(|location| location.height))
                .push_bind(location.map(|location| location.radius))
                .push_bind(self.mesh_hashes.get(&element.global_id).cloned())
                .push_bind(element.base_height);
        });
        query.build().execute(&mut self.connection).await?;

        let mut search = QueryBuilder::<Sqlite>::new(
            "INSERT INTO element_search (rowid, global_id, name, class, property_values) ",
        );
        search.push_values(rows, |mut row, (element_row, element)| {
            row.push_bind(*element_row)
                .push_bind(element.global_id.clone())
                .push_bind(element.name.as_deref().map(with_word_parts))
                .push_bind(with_word_parts(&element.class))
                .push_bind(indexed_property_values(&element.properties));
        });
        search.build().execute(&mut self.connection).await?;
        Ok(())
    }

    async fn insert_properties(
        &mut self,
        rows: &[(i64, &ElementProperty)],
    ) -> Result<(), sqlx::Error> {
        for statement_rows in rows.chunks(PROPERTY_ROWS_PER_STATEMENT) {
            let mut query = QueryBuilder::<Sqlite>::new(
                "INSERT INTO properties (element_row, key, value_text, value_number, value_boolean, units) ",
            );
            query.push_values(statement_rows, |mut row, (element_row, property)| {
                let (text, number, boolean) = match &property.value {
                    PropertyValue::Text(text) => (Some(text.clone()), None, None),
                    PropertyValue::Number(number) => (None, Some(*number), None),
                    PropertyValue::Boolean(boolean) => (None, None, Some(*boolean)),
                };
                row.push_bind(*element_row)
                    .push_bind(property.key.clone())
                    .push_bind(text)
                    .push_bind(number)
                    .push_bind(boolean)
                    .push_bind(property.units.clone());
            });
            query.build().execute(&mut self.connection).await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), sqlx::Error> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let elements = std::mem::take(&mut self.pending);
        self.pending_property_rows = 0;
        self.pending_text_bytes = 0;
        let first_row = self.next_row;
        self.next_row += elements.len() as i64;

        let element_rows: Vec<(i64, &ExtractedElement)> = (first_row..).zip(&elements).collect();
        self.insert_elements(&element_rows).await?;

        let property_rows: Vec<(i64, &ElementProperty)> = element_rows
            .iter()
            .flat_map(|(element_row, element)| {
                element
                    .properties
                    .iter()
                    .map(move |property| (*element_row, property))
            })
            .collect();
        self.insert_properties(&property_rows).await
    }

    async fn finish(
        mut self,
        warnings: &ExtractionWarnings,
        requested_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), sqlx::Error> {
        self.flush().await?;
        for (name, count) in warnings {
            sqlx::query("INSERT INTO warnings (name, count) VALUES (?, ?)")
                .bind(name.to_string())
                .bind(*count as i64)
                .execute(&mut self.connection)
                .await?;
        }
        sqlx::query("INSERT INTO build (requested_at, updated_at) VALUES (?, ?)")
            .bind(requested_at.to_rfc3339())
            .bind(chrono::Utc::now().to_rfc3339())
            .execute(&mut self.connection)
            .await?;
        for statement in INDEXES {
            sqlx::query(*statement)
                .execute(&mut self.connection)
                .await?;
        }
        sqlx::query("COMMIT").execute(&mut self.connection).await?;
        self.connection.close().await
    }
}

fn write_failure(error: sqlx::Error) -> String {
    format!("the element store could not be written: {error}")
}

pub fn build_store_and_label_meshes(
    source: &[u8],
    asset_dir: &Path,
    origin: ModelOrigin,
    east_north_up_placement: Option<&MapConversionPlacement>,
    mut meshes: Vec<MeshData>,
) -> Result<Vec<MeshData>, String> {
    let locations = element_locations(
        meshes
            .iter()
            .filter_map(|mesh| Some((mesh.asset_id.as_deref()?, mesh.positions.as_slice()))),
        origin,
    );
    let mesh_hashes = element_mesh_hashes(&meshes);

    std::fs::create_dir_all(asset_dir)
        .map_err(|error| format!("could not create {}: {error}", asset_dir.display()))?;
    let runtime = tokio::runtime::Handle::current();
    let building = asset_dir.join(BUILDING_FILE_NAME);
    remove_file_if_present(&building)?;
    let requested_at = chrono::Utc::now();

    let mut writer = runtime
        .block_on(StoreWriter::create(&building, mesh_hashes))
        .map_err(write_failure)?;
    let mut mesh_labels: HashMap<String, (String, Option<String>)> = HashMap::new();
    let warnings = extract_elements(
        source,
        &locations,
        origin,
        east_north_up_placement,
        |element| {
            if locations.contains_key(&element.global_id) {
                mesh_labels.insert(
                    element.global_id.clone(),
                    (element.class.clone(), element.storey.clone()),
                );
            }
            writer.push(element);
            if !writer.is_full() {
                return Ok(());
            }
            runtime
                .block_on(writer.flush())
                .map_err(|error| IngestError::ElementRefused(write_failure(error)))
        },
    )
    .map_err(|error| format!("element extraction failed: {error}"))?;
    runtime
        .block_on(writer.finish(&warnings, requested_at))
        .map_err(write_failure)?;

    for mesh in &mut meshes {
        let label = mesh.asset_id.as_deref().and_then(|id| mesh_labels.get(id));
        if let Some((class, storey)) = label {
            mesh.class = Some(class.clone());
            mesh.storey = storey.clone();
        }
    }
    Ok(meshes)
}

pub fn finish_build(asset_dir: &Path, tiling_succeeded: bool) -> Result<(), String> {
    let building = asset_dir.join(BUILDING_FILE_NAME);
    if !tiling_succeeded {
        return remove_file_if_present(&building);
    }
    if !building.exists() {
        return Ok(());
    }
    let describe =
        |error: std::io::Error| format!("could not publish {}: {error}", building.display());
    // the build ran with synchronous off, and a rename can reach disk before the file's pages do
    // windows refuses FlushFileBuffers on a handle opened read-only
    std::fs::OpenOptions::new()
        .write(true)
        .open(&building)
        .and_then(|file| file.sync_all())
        .map_err(describe)?;
    std::fs::rename(&building, store_path(asset_dir)).map_err(describe)
}

#[derive(Debug, Clone, Serialize)]
pub struct GeographicPoint {
    pub longitude: f64,
    pub latitude: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Location {
    pub center: GeographicPoint,
    pub radius: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementSummary {
    pub id: String,
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    pub ifc_class: String,
    pub storey: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementPropertyView {
    pub display_value: Option<String>,
    pub value: serde_json::Value,
    pub units: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementDetails {
    #[serde(flatten)]
    pub summary: ElementSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_height: Option<f64>,
    pub parent_id: Option<String>,
    pub properties: BTreeMap<String, ElementPropertyView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    pub properties: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeNode {
    pub kind: String,
    pub key: String,
    pub label: String,
    pub has_children: bool,
    pub id: String,
    pub ifc_class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_height: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreePathEntry {
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Aggregation {
    pub property_key: String,
    pub count: i64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub avg: Option<f64>,
    pub sum: f64,
    pub units: Vec<String>,
}

impl Aggregation {
    fn empty(property_key: &str) -> Self {
        Self {
            property_key: property_key.to_string(),
            count: 0,
            min: None,
            max: None,
            avg: None,
            sum: 0.0,
            units: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AggregationGroup {
    pub value: serde_json::Value,
    pub total: i64,
    pub aggregations: Vec<Aggregation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PropertyName {
    pub name: String,
    #[serde(rename = "type")]
    pub value_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildWarning {
    pub name: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyIndexStatus {
    pub status: &'static str,
    pub requested_at: String,
    pub updated_at: String,
    pub warnings: Vec<BuildWarning>,
}

pub(crate) async fn open_immutable_file(path: &Path) -> Result<SqliteConnection, sqlx::Error> {
    // the file is only ever replaced by rename, so a reader needs no locks or journal
    SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .immutable(true)
        .connect()
        .await
}

pub async fn open_store(asset_dir: &Path) -> Result<Option<SqliteConnection>, sqlx::Error> {
    let path = store_path(asset_dir);
    if !path.is_file() {
        return Ok(None);
    }
    let mut connection = open_immutable_file(&path).await?;
    let deadline = Instant::now() + QUERY_TIME_LIMIT;
    connection
        .lock_handle()
        .await?
        .set_progress_handler(VIRTUAL_MACHINE_STEPS_BETWEEN_CLOCK_READS, move || {
            Instant::now() < deadline
        });
    Ok(Some(connection))
}

const ELEMENT_COLUMNS: &str = "e.element_row, e.global_id, e.name, e.class, e.storey, e.longitude, e.latitude, e.height, e.radius";

fn location_from(row: &SqliteRow) -> Result<Option<Location>, sqlx::Error> {
    let longitude: Option<f64> = row.try_get("longitude")?;
    let latitude: Option<f64> = row.try_get("latitude")?;
    let height: Option<f64> = row.try_get("height")?;
    let radius: Option<f64> = row.try_get("radius")?;
    let (Some(longitude), Some(latitude), Some(height), Some(radius)) =
        (longitude, latitude, height, radius)
    else {
        return Ok(None);
    };
    Ok(Some(Location {
        center: GeographicPoint {
            longitude,
            latitude,
            height,
        },
        radius,
    }))
}

fn summary_from(row: &SqliteRow) -> Result<ElementSummary, sqlx::Error> {
    Ok(ElementSummary {
        id: row.try_get("global_id")?,
        label: row.try_get("name")?,
        location: location_from(row)?,
        ifc_class: row.try_get("class")?,
        storey: row.try_get("storey")?,
    })
}

pub struct ElementPage {
    pub items: Vec<ElementSummary>,
    pub total: i64,
    pub next_after_row: Option<i64>,
}

pub async fn list_elements(
    connection: &mut SqliteConnection,
    filter: &ElementFilter,
    after_row: i64,
    top: i64,
) -> Result<ElementPage, sqlx::Error> {
    let mut count = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) AS total FROM elements e WHERE ");
    filter.push_sql(&mut count);
    let total: i64 = count
        .build()
        .fetch_one(&mut *connection)
        .await?
        .try_get("total")?;

    let mut page = QueryBuilder::<Sqlite>::new("SELECT ");
    page.push(ELEMENT_COLUMNS).push(" FROM elements e WHERE ");
    filter.push_sql(&mut page);
    page.push(" AND e.element_row > ")
        .push_bind(after_row)
        .push(" ORDER BY e.element_row LIMIT ")
        .push_bind(top + 1);
    let rows = page.build().fetch_all(&mut *connection).await?;

    let has_more = rows.len() as i64 > top;
    let mut items = Vec::with_capacity(rows.len());
    let mut last_row = None;
    for row in rows.iter().take(top as usize) {
        items.push(summary_from(row)?);
        last_row = Some(row.try_get::<i64, _>("element_row")?);
    }
    Ok(ElementPage {
        items,
        total,
        next_after_row: last_row.filter(|_| has_more),
    })
}

fn property_view(row: &SqliteRow) -> Result<ElementPropertyView, sqlx::Error> {
    let text: Option<String> = row.try_get("value_text")?;
    let number: Option<f64> = row.try_get("value_number")?;
    let boolean: Option<bool> = row.try_get("value_boolean")?;
    let units: Option<String> = row.try_get("units")?;
    let display_value = match (&text, number, boolean) {
        (Some(text), _, _) => Some(text.clone()),
        (None, Some(number), _) => Some(match &units {
            Some(units) => format!("{number} {units}"),
            None => number.to_string(),
        }),
        (None, None, Some(boolean)) => Some(boolean.to_string()),
        (None, None, None) => None,
    };
    Ok(ElementPropertyView {
        display_value,
        value: scalar_json(text, number, boolean),
        units,
    })
}

fn scalar_json(
    text: Option<String>,
    number: Option<f64>,
    boolean: Option<bool>,
) -> serde_json::Value {
    match (text, number, boolean) {
        (Some(text), _, _) => serde_json::Value::String(text),
        (None, Some(number), _) => serde_json::json!(number),
        (None, None, Some(boolean)) => serde_json::Value::Bool(boolean),
        (None, None, None) => serde_json::Value::Null,
    }
}

pub async fn get_element(
    connection: &mut SqliteConnection,
    global_id: &str,
) -> Result<Option<ElementDetails>, sqlx::Error> {
    let mut query = QueryBuilder::<Sqlite>::new("SELECT ");
    query
        .push(ELEMENT_COLUMNS)
        .push(
            ", e.base_height, parent.global_id AS parent_id FROM elements e \
             LEFT JOIN elements parent ON parent.express_id = e.parent_express_id \
             WHERE e.global_id = ",
        )
        .push_bind(global_id.to_string())
        .push(" LIMIT 1");
    let Some(row) = query.build().fetch_optional(&mut *connection).await? else {
        return Ok(None);
    };
    let element_row: i64 = row.try_get("element_row")?;

    let property_rows = sqlx::query(
        "SELECT key, value_text, value_number, value_boolean, units FROM properties \
         WHERE element_row = ? ORDER BY rowid",
    )
    .bind(element_row)
    .fetch_all(&mut *connection)
    .await?;
    let mut properties = BTreeMap::new();
    for property in &property_rows {
        let key: String = property.try_get("key")?;
        properties.insert(key, property_view(property)?);
    }

    Ok(Some(ElementDetails {
        summary: summary_from(&row)?,
        base_height: row.try_get("base_height")?,
        parent_id: row.try_get("parent_id")?,
        properties,
    }))
}

pub struct SearchPage {
    pub items: Vec<SearchHit>,
    pub total: i64,
    pub next_offset: Option<i64>,
}

pub async fn search_elements(
    connection: &mut SqliteConnection,
    fts_query: &str,
    offset: i64,
) -> Result<SearchPage, sqlx::Error> {
    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM element_search WHERE element_search MATCH ?")
            .bind(fts_query.to_string())
            .fetch_one(&mut *connection)
            .await?;
    let rows = sqlx::query(
        "SELECT e.global_id, e.name, e.class, e.longitude, e.latitude, e.height, e.radius \
         FROM (SELECT rowid AS element_row, rank AS score FROM element_search \
               WHERE element_search MATCH ? ORDER BY rank LIMIT ? OFFSET ?) hit \
         JOIN elements e ON e.element_row = hit.element_row \
         ORDER BY hit.score",
    )
    .bind(fts_query.to_string())
    .bind(SEARCH_PAGE_SIZE + 1)
    .bind(offset)
    .fetch_all(&mut *connection)
    .await?;

    let has_more = rows.len() as i64 > SEARCH_PAGE_SIZE;
    let mut items = Vec::with_capacity(rows.len());
    for row in rows.iter().take(SEARCH_PAGE_SIZE as usize) {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "className".to_string(),
            serde_json::Value::String(row.try_get("class")?),
        );
        if let Some(name) = row.try_get::<Option<String>, _>("name")? {
            properties.insert("userLabel".to_string(), serde_json::Value::String(name));
        }
        items.push(SearchHit {
            id: row.try_get("global_id")?,
            location: location_from(row)?,
            properties,
        });
    }
    Ok(SearchPage {
        items,
        total,
        next_offset: has_more.then_some(offset + SEARCH_PAGE_SIZE),
    })
}

const STATISTICS_COLUMNS: &str = "p.key AS property_key, COUNT(p.value_number) AS value_count, \
     MIN(p.value_number) AS minimum, MAX(p.value_number) AS maximum, \
     AVG(p.value_number) AS average, TOTAL(p.value_number) AS total_value, \
     json_group_array(DISTINCT p.units) FILTER (WHERE p.value_number IS NOT NULL AND p.units IS NOT NULL) AS units_json";

fn aggregation_from(row: &SqliteRow) -> Result<Aggregation, sqlx::Error> {
    let units_json: String = row.try_get("units_json")?;
    let mut units: Vec<String> =
        serde_json::from_str(&units_json).map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
    units.sort();
    Ok(Aggregation {
        property_key: row.try_get("property_key")?,
        count: row.try_get("value_count")?,
        min: row.try_get("minimum")?,
        max: row.try_get("maximum")?,
        avg: row.try_get("average")?,
        sum: row.try_get("total_value")?,
        units,
    })
}

fn push_key_list(query: &mut QueryBuilder<Sqlite>, keys: &[String]) {
    query.push("(");
    let mut separated = query.separated(", ");
    for key in keys {
        separated.push_bind(key.clone());
    }
    query.push(")");
}

fn in_request_order(keys: &[String], mut found: HashMap<String, Aggregation>) -> Vec<Aggregation> {
    keys.iter()
        .map(|key| found.remove(key).unwrap_or_else(|| Aggregation::empty(key)))
        .collect()
}

pub async fn aggregate_elements(
    connection: &mut SqliteConnection,
    filter: &ElementFilter,
    keys: &[String],
) -> Result<(i64, Vec<Aggregation>), sqlx::Error> {
    let mut count = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) AS total FROM elements e WHERE ");
    filter.push_sql(&mut count);
    let total: i64 = count
        .build()
        .fetch_one(&mut *connection)
        .await?
        .try_get("total")?;
    if keys.is_empty() {
        return Ok((total, Vec::new()));
    }

    let mut statistics = QueryBuilder::<Sqlite>::new("SELECT ");
    statistics
        .push(STATISTICS_COLUMNS)
        .push(" FROM properties p WHERE p.key IN ");
    push_key_list(&mut statistics, keys);
    statistics.push(" AND p.element_row IN (SELECT e.element_row FROM elements e WHERE ");
    filter.push_sql(&mut statistics);
    statistics.push(") GROUP BY p.key");
    let rows = statistics.build().fetch_all(&mut *connection).await?;

    let mut found = HashMap::new();
    for row in &rows {
        let aggregation = aggregation_from(row)?;
        found.insert(aggregation.property_key.clone(), aggregation);
    }
    Ok((total, in_request_order(keys, found)))
}

fn push_group_select(query: &mut QueryBuilder<Sqlite>, group_by: &GroupBy) {
    match group_by {
        GroupBy::Root(field) => {
            query
                .push("SELECT ")
                .push(field.column())
                .push(" AS group_text, NULL AS group_number, NULL AS group_boolean");
        }
        GroupBy::Property(_) => {
            query.push(
                "SELECT g.value_text AS group_text, g.value_number AS group_number, \
                 g.value_boolean AS group_boolean",
            );
        }
    }
}

fn push_group_source(query: &mut QueryBuilder<Sqlite>, group_by: &GroupBy) {
    query.push(" FROM elements e");
    if let GroupBy::Property(key) = group_by {
        query
            .push(" LEFT JOIN properties g ON g.element_row = e.element_row AND g.key = ")
            .push_bind(key.clone());
    }
}

type GroupIdentity = (Option<String>, Option<u64>, Option<bool>);

fn group_identity(row: &SqliteRow) -> Result<GroupIdentity, sqlx::Error> {
    Ok((
        row.try_get("group_text")?,
        row.try_get::<Option<f64>, _>("group_number")?
            .map(f64::to_bits),
        row.try_get("group_boolean")?,
    ))
}

pub async fn count_element_groups(
    connection: &mut SqliteConnection,
    filter: &ElementFilter,
    group_by: &GroupBy,
) -> Result<Vec<SqliteRow>, sqlx::Error> {
    let mut query = QueryBuilder::<Sqlite>::new("");
    push_group_select(&mut query, group_by);
    query.push(", COUNT(*) AS group_total");
    push_group_source(&mut query, group_by);
    query.push(" WHERE ");
    filter.push_sql(&mut query);
    query
        .push(" GROUP BY 1, 2, 3 ORDER BY 1, 2, 3 LIMIT ")
        .push_bind(MAX_GROUPS as i64 + 1);
    query.build().fetch_all(&mut *connection).await
}

pub async fn aggregate_groups(
    connection: &mut SqliteConnection,
    filter: &ElementFilter,
    group_by: &GroupBy,
    group_rows: &[SqliteRow],
    keys: &[String],
) -> Result<Vec<AggregationGroup>, sqlx::Error> {
    let mut found_by_group: HashMap<GroupIdentity, HashMap<String, Aggregation>> = HashMap::new();
    if !keys.is_empty() {
        let mut query = QueryBuilder::<Sqlite>::new("");
        push_group_select(&mut query, group_by);
        query.push(", ").push(STATISTICS_COLUMNS);
        push_group_source(&mut query, group_by);
        query.push(" JOIN properties p ON p.element_row = e.element_row AND p.key IN ");
        push_key_list(&mut query, keys);
        query.push(" WHERE ");
        filter.push_sql(&mut query);
        query.push(" GROUP BY 1, 2, 3, 4");
        let rows = query.build().fetch_all(&mut *connection).await?;
        for row in &rows {
            let aggregation = aggregation_from(row)?;
            found_by_group
                .entry(group_identity(row)?)
                .or_default()
                .insert(aggregation.property_key.clone(), aggregation);
        }
    }

    let mut groups = Vec::with_capacity(group_rows.len());
    for row in group_rows {
        let found = found_by_group
            .remove(&group_identity(row)?)
            .unwrap_or_default();
        groups.push(AggregationGroup {
            value: scalar_json(
                row.try_get("group_text")?,
                row.try_get("group_number")?,
                row.try_get("group_boolean")?,
            ),
            total: row.try_get("group_total")?,
            aggregations: in_request_order(keys, found),
        });
    }
    Ok(groups)
}

pub async fn list_property_names(
    connection: &mut SqliteConnection,
    limit: i64,
    offset: i64,
) -> Result<(Vec<PropertyName>, i64), sqlx::Error> {
    const DISTINCT_NAMES: &str = "SELECT DISTINCT key, CASE \
         WHEN value_number IS NOT NULL THEN 'number' \
         WHEN value_boolean IS NOT NULL THEN 'boolean' \
         ELSE 'string' END AS value_type FROM properties";
    let mut count = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) AS total FROM (");
    count.push(DISTINCT_NAMES).push(")");
    let total: i64 = count
        .build()
        .fetch_one(&mut *connection)
        .await?
        .try_get("total")?;

    let mut page = QueryBuilder::<Sqlite>::new("");
    page.push(DISTINCT_NAMES)
        .push(" ORDER BY key, value_type LIMIT ")
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(offset);
    let rows = page.build().fetch_all(&mut *connection).await?;
    let mut names = Vec::with_capacity(rows.len());
    for row in &rows {
        names.push(PropertyName {
            name: row.try_get("key")?,
            value_type: row.try_get("value_type")?,
        });
    }
    Ok((names, total))
}

pub async fn property_index_status(
    connection: &mut SqliteConnection,
) -> Result<PropertyIndexStatus, sqlx::Error> {
    let build = sqlx::query("SELECT requested_at, updated_at FROM build")
        .fetch_one(&mut *connection)
        .await?;
    let warning_rows = sqlx::query("SELECT name, count FROM warnings ORDER BY name")
        .fetch_all(&mut *connection)
        .await?;
    let mut warnings = Vec::with_capacity(warning_rows.len());
    for row in &warning_rows {
        warnings.push(BuildWarning {
            name: row.try_get("name")?,
            count: row.try_get("count")?,
        });
    }
    Ok(PropertyIndexStatus {
        status: "Completed",
        requested_at: build.try_get("requested_at")?,
        updated_at: build.try_get("updated_at")?,
        warnings,
    })
}

const TREE_NODE_COLUMNS: &str = "SELECT e.global_id, e.name, e.class, e.tree_kind, e.base_height, \
     EXISTS (SELECT 1 FROM elements child WHERE child.parent_express_id = e.express_id) AS has_children \
     FROM elements e WHERE ";

pub async fn tree_children(
    connection: &mut SqliteConnection,
    parent_global_id: Option<&str>,
) -> Result<Option<Vec<TreeNode>>, sqlx::Error> {
    let mut query = QueryBuilder::<Sqlite>::new(TREE_NODE_COLUMNS);
    match parent_global_id {
        None => {
            query.push("e.parent_express_id IS NULL");
        }
        Some(global_id) => {
            let parent = sqlx::query("SELECT express_id FROM elements WHERE global_id = ? LIMIT 1")
                .bind(global_id.to_string())
                .fetch_optional(&mut *connection)
                .await?;
            let Some(parent) = parent else {
                return Ok(None);
            };
            query
                .push("e.parent_express_id = ")
                .push_bind(parent.try_get::<i64, _>("express_id")?);
        }
    }
    query
        .push(" ORDER BY e.element_row LIMIT ")
        .push_bind(MAX_TREE_CHILDREN as i64 + 1);
    let rows = query.build().fetch_all(&mut *connection).await?;

    let mut nodes = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: String = row.try_get("global_id")?;
        let ifc_class: String = row.try_get("class")?;
        let name: Option<String> = row.try_get("name")?;
        nodes.push(TreeNode {
            kind: row.try_get("tree_kind")?,
            key: id.clone(),
            label: name.unwrap_or_else(|| ifc_class.clone()),
            has_children: row.try_get::<i64, _>("has_children")? != 0,
            id,
            ifc_class,
            base_height: row.try_get("base_height")?,
        });
    }
    Ok(Some(nodes))
}

pub async fn tree_path(
    connection: &mut SqliteConnection,
    global_id: &str,
) -> Result<Vec<TreePathEntry>, sqlx::Error> {
    let rows = sqlx::query(
        "WITH RECURSIVE chain (express_id, depth) AS ( \
             SELECT express_id, 0 FROM elements WHERE global_id = ? \
             UNION \
             SELECT e.parent_express_id, chain.depth + 1 FROM chain \
             JOIN elements e ON e.express_id = chain.express_id \
             WHERE e.parent_express_id IS NOT NULL AND chain.depth < ? \
         ) \
         SELECT e.global_id, e.tree_kind FROM chain \
         JOIN elements e ON e.express_id = chain.express_id \
         ORDER BY chain.depth DESC",
    )
    .bind(global_id.to_string())
    .bind(MAX_TREE_DEPTH)
    .fetch_all(&mut *connection)
    .await?;
    let mut path = Vec::with_capacity(rows.len());
    for row in &rows {
        path.push(TreePathEntry {
            kind: row.try_get("tree_kind")?,
            id: row.try_get("global_id")?,
        });
    }
    Ok(path)
}

const GLOBAL_IDS_PER_LOOKUP: usize = 512;

pub(crate) async fn known_global_ids(
    connection: &mut SqliteConnection,
    global_ids: &[String],
) -> Result<HashSet<String>, sqlx::Error> {
    let mut known = HashSet::new();
    for chunk in global_ids.chunks(GLOBAL_IDS_PER_LOOKUP) {
        let mut query =
            QueryBuilder::<Sqlite>::new("SELECT global_id FROM elements WHERE global_id IN ");
        push_key_list(&mut query, chunk);
        for row in query.build().fetch_all(&mut *connection).await? {
            known.insert(row.try_get("global_id")?);
        }
    }
    Ok(known)
}

#[derive(Debug, PartialEq)]
pub(crate) struct ComparableValue {
    text: Option<String>,
    number: Option<f64>,
    boolean: Option<bool>,
    units: Option<String>,
}

pub(crate) struct ComparableElement {
    pub(crate) global_id: String,
    pub(crate) label: Option<String>,
    pub(crate) ifc_class: String,
    pub(crate) storey: Option<String>,
    pub(crate) parent_id: Option<String>,
    pub(crate) location: Option<Location>,
    pub(crate) mesh_hash: Option<String>,
    pub(crate) properties: BTreeMap<String, ComparableValue>,
}

const ALL_ELEMENTS_BY_GLOBAL_ID: &str = "SELECT e.global_id, e.name, e.class, e.storey, \
     e.longitude, e.latitude, e.height, e.radius, e.mesh_hash, parent.global_id AS parent_id, \
     p.key AS property_key, p.value_text, p.value_number, p.value_boolean, p.units \
     FROM elements e \
     LEFT JOIN elements parent ON parent.express_id = e.parent_express_id \
     LEFT JOIN properties p ON p.element_row = e.element_row \
     ORDER BY e.global_id, p.key";

pub(crate) struct ElementsInGlobalIdOrder<'connection> {
    rows: BoxStream<'connection, Result<SqliteRow, sqlx::Error>>,
    row_of_next_element: Option<SqliteRow>,
}

fn add_property(element: &mut ComparableElement, row: &SqliteRow) -> Result<(), sqlx::Error> {
    let Some(key) = row.try_get::<Option<String>, _>("property_key")? else {
        return Ok(());
    };
    element.properties.insert(
        key,
        ComparableValue {
            text: row.try_get("value_text")?,
            number: row.try_get("value_number")?,
            boolean: row.try_get("value_boolean")?,
            units: row.try_get("units")?,
        },
    );
    Ok(())
}

impl<'connection> ElementsInGlobalIdOrder<'connection> {
    pub(crate) fn new(connection: &'connection mut SqliteConnection) -> Self {
        Self {
            rows: sqlx::query(ALL_ELEMENTS_BY_GLOBAL_ID).fetch(connection),
            row_of_next_element: None,
        }
    }

    pub(crate) async fn next_element(&mut self) -> Result<Option<ComparableElement>, sqlx::Error> {
        let first_row = match self.row_of_next_element.take() {
            Some(row) => Some(row),
            None => self.rows.try_next().await?,
        };
        let Some(first_row) = first_row else {
            return Ok(None);
        };
        let mut element = ComparableElement {
            global_id: first_row.try_get("global_id")?,
            label: first_row.try_get("name")?,
            ifc_class: first_row.try_get("class")?,
            storey: first_row.try_get("storey")?,
            parent_id: first_row.try_get("parent_id")?,
            location: location_from(&first_row)?,
            mesh_hash: first_row.try_get("mesh_hash")?,
            properties: BTreeMap::new(),
        };
        add_property(&mut element, &first_row)?;
        while let Some(row) = self.rows.try_next().await? {
            if row.try_get::<String, _>("global_id")? != element.global_id {
                self.row_of_next_element = Some(row);
                break;
            }
            add_property(&mut element, &row)?;
        }
        Ok(Some(element))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_index_under_their_case_and_digit_parts() {
        assert_eq!(with_word_parts("FireRating2"), "FireRating2 Fire Rating 2");
        assert_eq!(with_word_parts("IfcWall"), "IfcWall Ifc Wall");
        assert_eq!(with_word_parts("plain words"), "plain words");
        assert_eq!(with_word_parts("a:B"), "a:B");
    }

    #[tokio::test]
    async fn pending_text_fills_the_writer_before_its_row_counts_do() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = StoreWriter::create(&dir.path().join("store.sqlite"), HashMap::new())
            .await
            .unwrap();
        let element_with_name_bytes = |bytes: usize| ExtractedElement {
            express_id: 1,
            global_id: "id".to_string(),
            class: "IfcWall".to_string(),
            name: Some("x".repeat(bytes)),
            tree_kind: tiletopia_ingest::ifc_elements::TreeKind::Element,
            parent_express_id: None,
            storey: None,
            location: None,
            base_height: None,
            properties: Vec::new(),
        };

        writer.push(element_with_name_bytes(TEXT_BYTES_PER_FLUSH / 2));
        assert!(!writer.is_full());

        writer.push(element_with_name_bytes(TEXT_BYTES_PER_FLUSH / 2));
        assert!(writer.is_full());

        writer.flush().await.unwrap();
        assert!(!writer.is_full());
    }
}

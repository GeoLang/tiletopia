use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Json, Response},
    routing::{get, put},
};
use serde::Deserialize;
use serde_json::json;
use sqlx::SqliteConnection;
use std::sync::Arc;
use tokio::sync::{Semaphore, SemaphorePermit};

use crate::dxf_export::mesh_source;
use crate::element_query::{parse_aggregation_keys, parse_filter, parse_group_by, parse_search};
use crate::element_store::{
    self, EditableProperty, ElementPropertyView, LoadedProperty, MAX_GROUPS, MAX_TREE_CHILDREN,
    PropertyStoreError, aggregate_elements, aggregate_groups, count_element_groups, get_element,
    list_elements, list_property_names, load_property, open_store, property_index_status,
    search_elements, store_edited_property, tree_children, tree_path,
};
use crate::ion_compat::{IonError, resolve_asset};
use crate::{AppState, Asset, AssetStatus, auth, may_modify_asset, may_view_asset, users};
use std::path::PathBuf;
use tiletopia_ingest::ifc_property_write::{
    MAX_TEXT_BYTES, PropertyLiteral, PropertyWriteError, rewrite_property,
};

const ROOT_NODE_KEY: &str = "root";
const DEFAULT_TOP: i64 = 100;
const MAX_TOP: i64 = 1000;
const DEFAULT_PROPERTY_LIMIT: i64 = 1000;
const MAX_PROPERTY_LIMIT: i64 = 1000;

// every open store runs on its own sqlite worker thread
static OPEN_STORE_SLOTS: Semaphore = Semaphore::const_new(16);

pub fn element_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/assets/{id}/elements", get(list))
        .route("/v1/assets/{id}/elements/search", get(search))
        .route("/v1/assets/{id}/elements/aggregate", get(aggregate))
        .route("/v1/assets/{id}/elements/properties", get(properties))
        .route(
            "/v1/assets/{id}/elements/property-index",
            get(property_index),
        )
        .route("/v1/assets/{id}/elements/{element_id}", get(get_one))
        .route(
            "/v1/assets/{id}/elements/{element_id}/properties",
            put(update_property).layer(middleware::from_fn(users::require_editor)),
        )
        .route("/v1/assets/{id}/tree/children", get(children))
        .route("/v1/assets/{id}/tree/parents", get(parents))
}

pub(crate) fn element_error(
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
) -> IonError {
    (
        status,
        Json(json!({ "code": code, "message": message.into() })),
    )
}

pub(crate) fn invalid_parameter(message: impl Into<String>) -> IonError {
    element_error(StatusCode::BAD_REQUEST, "INVALID_PARAMETER", message)
}

pub(crate) fn server_failure(message: &str, error: impl std::fmt::Display) -> IonError {
    tracing::error!("{message}: {error}");
    element_error(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", message)
}

fn internal_error(context: &str, error: impl std::fmt::Display) -> IonError {
    tracing::error!("{context}: {error}");
    element_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL_ERROR",
        "the element store could not be read",
    )
}

pub(crate) fn store_error(error: sqlx::Error) -> IonError {
    // sqlite answers SQLITE_INTERRUPT, code 9, when the progress handler stops a statement
    let was_stopped = error
        .as_database_error()
        .and_then(|database_error| database_error.code())
        .is_some_and(|code| code == "9");
    if was_stopped {
        return element_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "QUERY_TOO_EXPENSIVE",
            "the query ran for more than 10 seconds and was stopped",
        );
    }
    internal_error("element store query failed", error)
}

pub(crate) struct ReadableStore {
    pub(crate) connection: SqliteConnection,
    ion_id: i64,
    slot: SemaphorePermit<'static>,
}

async fn run_past_disconnect<T, Statements>(
    store: ReadableStore,
    statements: impl FnOnce(SqliteConnection) -> Statements,
) -> Result<T, IonError>
where
    T: Send + 'static,
    Statements: Future<Output = Result<T, IonError>> + Send + 'static,
{
    let statements = statements(store.connection);
    let slot = store.slot;
    // sqlx-sqlite notices a dropped caller only between rows
    tokio::spawn(async move {
        let outcome = statements.await;
        drop(slot);
        outcome
    })
    .await
    .map_err(|error| internal_error("element store task failed", error))?
}

pub(crate) async fn permitted_asset(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    permits: fn(&auth::Claims, Option<&str>) -> bool,
) -> Result<(Asset, i64), IonError> {
    let claims = users::claims_from_headers(headers).map_err(|status| {
        element_error(
            status,
            "UNAUTHORIZED",
            "element routes need Authorization: Bearer <token>",
        )
    })?;
    let resolved = resolve_asset(state, id)
        .await
        .map_err(|error| internal_error("asset lookup failed", error))?;
    // TODO: hidden and missing assets answer alike here, but the open GET /v1/assets lists every ion id
    let Some((asset, ion_id)) =
        resolved.filter(|(asset, _)| permits(&claims, asset.owner_id.as_deref()))
    else {
        return Err(element_error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            format!("no asset {id}"),
        ));
    };
    Ok((asset, ion_id))
}

pub(crate) async fn open_readable_store(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
) -> Result<ReadableStore, IonError> {
    let (asset, ion_id) = permitted_asset(state, headers, id, may_view_asset).await?;

    let slot = OPEN_STORE_SLOTS
        .acquire()
        .await
        .map_err(|error| internal_error("element store slot failed", error))?;
    let asset_dir = state.data_dir.join(asset.id.to_string());
    let connection = open_store(&asset_dir)
        .await
        .map_err(store_error)?
        .ok_or_else(|| no_element_store(id, &asset.status))?;
    Ok(ReadableStore {
        connection,
        ion_id,
        slot,
    })
}

fn no_element_store(id: &str, status: &AssetStatus) -> IonError {
    element_error(
        StatusCode::NOT_FOUND,
        "ASSET_HAS_NO_ELEMENT_STORE",
        format!(
            "asset {id} has no element store: only an IFC model gets one, when its tiling job finishes, and its status is {:?}",
            status
        ),
    )
}

pub(crate) fn bounded_integer(
    name: &str,
    raw: Option<&str>,
    default: i64,
    maximum: i64,
) -> Result<i64, IonError> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    raw.parse::<i64>()
        .ok()
        .filter(|value| (1..=maximum).contains(value))
        .ok_or_else(|| invalid_parameter(format!("{name} takes an integer from 1 to {maximum}")))
}

pub(crate) fn continuation_position(raw: Option<&str>) -> Result<i64, IonError> {
    let Some(raw) = raw else {
        return Ok(0);
    };
    raw.parse::<i64>()
        .ok()
        .filter(|position| *position >= 0)
        .ok_or_else(|| invalid_parameter("continuationToken is not one this server issued"))
}

fn filter_from(raw: Option<&str>) -> Result<crate::element_query::ElementFilter, IonError> {
    parse_filter(raw.unwrap_or("{}"))
        .map_err(|message| element_error(StatusCode::BAD_REQUEST, "INVALID_FILTER", message))
}

#[derive(Debug, Deserialize)]
struct ListParameters {
    filter: Option<String>,
    top: Option<String>,
    #[serde(rename = "continuationToken")]
    continuation_token: Option<String>,
}

async fn list(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(parameters): Query<ListParameters>,
) -> Result<Json<serde_json::Value>, IonError> {
    let store = open_readable_store(&state, &headers, &id).await?;
    let filter = filter_from(parameters.filter.as_deref())?;
    let top = bounded_integer("top", parameters.top.as_deref(), DEFAULT_TOP, MAX_TOP)?;
    let after_row = continuation_position(parameters.continuation_token.as_deref())?;

    run_past_disconnect(store, move |mut connection| async move {
        let page = list_elements(&mut connection, &filter, after_row, top)
            .await
            .map_err(store_error)?;
        let mut body = json!({ "items": page.items, "total": page.total });
        if let Some(next) = page.next_after_row {
            body["continuationToken"] = json!(next.to_string());
        }
        Ok::<_, IonError>(Json(body))
    })
    .await
}

async fn get_one(
    State(state): State<Arc<AppState>>,
    Path((id, element_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<element_store::ElementDetails>, IonError> {
    let store = open_readable_store(&state, &headers, &id).await?;
    run_past_disconnect(store, move |mut connection| async move {
        get_element(&mut connection, &element_id)
            .await
            .map_err(store_error)?
            .map(Json)
            .ok_or_else(|| {
                element_error(
                    StatusCode::NOT_FOUND,
                    "ELEMENT_NOT_FOUND",
                    format!("asset {id} has no element {element_id}"),
                )
            })
    })
    .await
}

#[derive(Debug, Deserialize)]
struct SearchParameters {
    search: Option<String>,
    #[serde(rename = "continuationToken")]
    continuation_token: Option<String>,
}

async fn search(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(parameters): Query<SearchParameters>,
) -> Result<Json<serde_json::Value>, IonError> {
    let store = open_readable_store(&state, &headers, &id).await?;
    let search_text = parameters
        .search
        .as_deref()
        .ok_or_else(|| invalid_parameter("search is required"))?;
    let fts_query = parse_search(search_text)
        .map_err(|message| element_error(StatusCode::BAD_REQUEST, "INVALID_SEARCH", message))?;
    let offset = continuation_position(parameters.continuation_token.as_deref())?;

    run_past_disconnect(store, move |mut connection| async move {
        let page = search_elements(&mut connection, &fts_query, offset)
            .await
            .map_err(store_error)?;
        let mut body = json!({ "total": page.total, "items": page.items });
        if let Some(next) = page.next_offset {
            body["continuationToken"] = json!(next.to_string());
        }
        Ok::<_, IonError>(Json(body))
    })
    .await
}

#[derive(Debug, Deserialize)]
struct AggregateParameters {
    filter: Option<String>,
    aggregations: Option<String>,
    #[serde(rename = "groupBy")]
    group_by: Option<String>,
}

async fn aggregate(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(parameters): Query<AggregateParameters>,
) -> Result<Json<serde_json::Value>, IonError> {
    let store = open_readable_store(&state, &headers, &id).await?;
    let filter = filter_from(parameters.filter.as_deref())?;
    let group_by = parameters
        .group_by
        .as_deref()
        .map(parse_group_by)
        .transpose()
        .map_err(invalid_parameter)?;
    let keys = match parameters.aggregations.as_deref() {
        Some(raw) => parse_aggregation_keys(raw).map_err(invalid_parameter)?,
        None if group_by.is_some() => Vec::new(),
        None => {
            return Err(invalid_parameter(
                "aggregations is required without groupBy",
            ));
        }
    };

    run_past_disconnect(store, move |mut connection| async move {
        let (total, aggregations) = aggregate_elements(&mut connection, &filter, &keys)
            .await
            .map_err(store_error)?;
        let mut body = json!({ "total": total, "aggregations": aggregations });
        if let Some(group_by) = group_by {
            let group_rows = count_element_groups(&mut connection, &filter, &group_by)
                .await
                .map_err(store_error)?;
            if group_rows.len() > MAX_GROUPS {
                return Err(element_error(
                    StatusCode::BAD_REQUEST,
                    "GROUP_LIMIT_EXCEEDED",
                    format!("groupBy found more than {MAX_GROUPS} distinct values"),
                ));
            }
            let groups = aggregate_groups(&mut connection, &filter, &group_by, &group_rows, &keys)
                .await
                .map_err(store_error)?;
            body["groups"] = json!(groups);
        }
        Ok::<_, IonError>(Json(body))
    })
    .await
}

#[derive(Debug, Deserialize)]
struct PropertiesParameters {
    limit: Option<String>,
    page: Option<String>,
}

async fn properties(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(parameters): Query<PropertiesParameters>,
) -> Result<Response, IonError> {
    let store = open_readable_store(&state, &headers, &id).await?;
    let ion_id = store.ion_id;
    let limit = bounded_integer(
        "limit",
        parameters.limit.as_deref(),
        DEFAULT_PROPERTY_LIMIT,
        MAX_PROPERTY_LIMIT,
    )?;
    let page = bounded_integer(
        "page",
        parameters.page.as_deref(),
        1,
        i64::MAX / MAX_PROPERTY_LIMIT,
    )?;

    let (names, total) = run_past_disconnect(store, move |mut connection| async move {
        list_property_names(&mut connection, limit, (page - 1) * limit)
            .await
            .map_err(store_error)
    })
    .await?;

    let page_link = |target: i64, relation: &str| {
        format!(
            "<{}/v1/assets/{ion_id}/elements/properties?limit={limit}&page={target}>; rel=\"{relation}\"",
            state.ion_base_url
        )
    };
    let mut links = Vec::new();
    if page > 1 {
        links.push(page_link(page - 1, "prev"));
    }
    if page * limit < total {
        links.push(page_link(page + 1, "next"));
    }

    let mut response = Json(json!({ "items": names, "total": total })).into_response();
    if !links.is_empty() {
        let value = HeaderValue::from_str(&links.join(", "))
            .map_err(|error| internal_error("property link header failed", error))?;
        response.headers_mut().insert(header::LINK, value);
    }
    Ok(response)
}

async fn property_index(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<element_store::PropertyIndexStatus>, IonError> {
    let store = open_readable_store(&state, &headers, &id).await?;
    run_past_disconnect(store, |mut connection| async move {
        property_index_status(&mut connection)
            .await
            .map(Json)
            .map_err(store_error)
    })
    .await
}

#[derive(Debug, Deserialize)]
struct ChildrenParameters {
    #[serde(rename = "nodeKey")]
    node_key: Option<String>,
}

async fn children(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(parameters): Query<ChildrenParameters>,
) -> Result<Json<serde_json::Value>, IonError> {
    let store = open_readable_store(&state, &headers, &id).await?;
    run_past_disconnect(store, move |mut connection| async move {
        let parent = parameters
            .node_key
            .as_deref()
            .filter(|key| *key != ROOT_NODE_KEY);
        let nodes = tree_children(&mut connection, parent)
            .await
            .map_err(store_error)?
            .ok_or_else(|| invalid_parameter("nodeKey names no node in this asset"))?;
        if nodes.len() > MAX_TREE_CHILDREN {
            return Err(element_error(
                StatusCode::BAD_REQUEST,
                "TREE_LIMIT_EXCEEDED",
                format!("this node has more than {MAX_TREE_CHILDREN} children"),
            ));
        }
        Ok::<_, IonError>(Json(json!({ "nodes": nodes })))
    })
    .await
}

#[derive(Debug, Deserialize)]
struct ParentsParameters {
    #[serde(rename = "elementId")]
    element_id: Option<String>,
}

async fn parents(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(parameters): Query<ParentsParameters>,
) -> Result<Json<serde_json::Value>, IonError> {
    let store = open_readable_store(&state, &headers, &id).await?;
    let element_id = parameters
        .element_id
        .ok_or_else(|| invalid_parameter("elementId is required"))?;
    run_past_disconnect(store, move |mut connection| async move {
        let path = tree_path(&mut connection, &element_id)
            .await
            .map_err(store_error)?;
        if path.is_empty() {
            return Err(element_error(
                StatusCode::NOT_FOUND,
                "ELEMENT_NOT_FOUND",
                format!("asset {id} has no element {element_id}"),
            ));
        }
        Ok::<_, IonError>(Json(json!({ "path": path })))
    })
    .await
}

#[derive(Debug, Deserialize)]
struct PropertyUpdate {
    key: String,
    value: serde_json::Value,
}

async fn update_property(
    State(state): State<Arc<AppState>>,
    Path((id, element_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(update): Json<PropertyUpdate>,
) -> Result<Json<ElementPropertyView>, IonError> {
    if update.key.is_empty() {
        return Err(invalid_parameter("key is empty"));
    }
    if update.value.is_null() {
        return Err(invalid_parameter("a property value cannot be null"));
    }
    let (asset, _) = permitted_asset(&state, &headers, &id, may_modify_asset).await?;
    let slot = OPEN_STORE_SLOTS
        .acquire()
        .await
        .map_err(|error| internal_error("element store slot failed", error))?;
    let asset_dir = state.data_dir.join(asset.id.to_string());
    let mut connection = open_store(&asset_dir)
        .await
        .map_err(store_error)?
        .ok_or_else(|| no_element_store(&id, &asset.status))?;
    let loaded = load_property(&mut connection, &element_id, &update.key)
        .await
        .map_err(store_error)?;
    drop(connection);
    drop(slot);

    let found = match loaded {
        LoadedProperty::NoElement => {
            return Err(element_error(
                StatusCode::NOT_FOUND,
                "ELEMENT_NOT_FOUND",
                format!("asset {id} has no element {element_id}"),
            ));
        }
        LoadedProperty::NoProperty => {
            return Err(element_error(
                StatusCode::NOT_FOUND,
                "PROPERTY_NOT_FOUND",
                format!(
                    "asset {id} has no property {} on element {element_id}",
                    update.key
                ),
            ));
        }
        LoadedProperty::Found(found) => found,
    };
    let literal = stored_literal(&found, &update.key, &update.value)?;
    let express_id = u32::try_from(found.express_id).map_err(|_| ifc_element_gone(&element_id))?;
    let source = mesh_source(&state, &asset, &id).await?;
    if source.extension != "ifc" {
        return Err(element_error(
            StatusCode::CONFLICT,
            "SOURCE_IS_NOT_IFC",
            format!(
                "asset {id}'s source is a .{}, which has no IFC properties to change",
                source.extension
            ),
        ));
    }

    let path = PathBuf::from(source.job.input_path);
    let key = update.key.clone();
    let rewritten =
        tokio::task::spawn_blocking(move || rewrite_on_disk(path, express_id, key, literal))
            .await
            .map_err(|error| server_failure("the property could not be written", error))?;
    let (path, previous) = match rewritten {
        Ok(written) => written,
        Err(WriteFailure::Io(error)) => {
            return Err(server_failure("the property could not be written", error));
        }
        Err(WriteFailure::Refused(error)) => {
            return Err(refused_property(&update.key, &element_id, error));
        }
    };

    let (text, number, boolean) = columns_for(&update.value);
    match store_edited_property(
        &asset_dir,
        found.element_row,
        &update.key,
        text,
        number,
        boolean,
    )
    .await
    {
        Ok(view) => Ok(Json(view)),
        Err(error) => {
            let restored = tokio::task::spawn_blocking(move || restore_ifc(&path, &previous)).await;
            if let Err(error) = restored.unwrap_or_else(|error| Err(std::io::Error::other(error))) {
                tracing::error!(
                    "the IFC could not be restored after the element store refused the edit: {error}"
                );
            }
            Err(stored_property_failure(
                &id,
                &element_id,
                &update.key,
                error,
            ))
        }
    }
}

fn stored_literal(
    property: &EditableProperty,
    key: &str,
    value: &serde_json::Value,
) -> Result<PropertyLiteral, IonError> {
    match (&property.text, property.number, property.boolean, value) {
        (Some(_), _, _, serde_json::Value::String(text)) => {
            if text.len() > MAX_TEXT_BYTES {
                return Err(invalid_parameter(format!(
                    "a property value is at most {MAX_TEXT_BYTES} bytes"
                )));
            }
            if text.chars().any(char::is_control) {
                return Err(invalid_parameter("the value contains a control character"));
            }
            Ok(PropertyLiteral::Text(text.clone()))
        }
        (None, Some(_), _, serde_json::Value::Number(number)) => {
            let number = number
                .as_f64()
                .filter(|number| number.is_finite())
                .ok_or_else(|| invalid_parameter("a property value must be a finite number"))?;
            Ok(PropertyLiteral::Number(number))
        }
        (None, None, Some(_), serde_json::Value::Bool(boolean)) => {
            Ok(PropertyLiteral::Boolean(*boolean))
        }
        _ => Err(element_error(
            StatusCode::BAD_REQUEST,
            "KIND_MISMATCH",
            format!("property {key} keeps the kind stored in the IFC"),
        )),
    }
}

fn columns_for(value: &serde_json::Value) -> (Option<String>, Option<f64>, Option<bool>) {
    match value {
        serde_json::Value::String(text) => (Some(text.clone()), None, None),
        serde_json::Value::Number(number) => (None, number.as_f64(), None),
        serde_json::Value::Bool(boolean) => (None, None, Some(*boolean)),
        _ => (None, None, None),
    }
}

enum WriteFailure {
    Io(std::io::Error),
    Refused(PropertyWriteError),
}

fn rewrite_on_disk(
    path: PathBuf,
    express_id: u32,
    key: String,
    value: PropertyLiteral,
) -> Result<(PathBuf, Vec<u8>), WriteFailure> {
    let previous = std::fs::read(&path).map_err(WriteFailure::Io)?;
    let rewritten =
        rewrite_property(&previous, express_id, &key, &value).map_err(WriteFailure::Refused)?;
    let writing = writing_path(&path).map_err(WriteFailure::Io)?;
    std::fs::write(&writing, &rewritten).map_err(WriteFailure::Io)?;
    if let Err(error) = std::fs::rename(&writing, &path) {
        let _ = std::fs::remove_file(&writing);
        return Err(WriteFailure::Io(error));
    }
    Ok((path, previous))
}

fn restore_ifc(path: &std::path::Path, previous: &[u8]) -> std::io::Result<()> {
    let writing = writing_path(path)?;
    std::fs::write(&writing, previous)?;
    if let Err(error) = std::fs::rename(&writing, path) {
        let _ = std::fs::remove_file(&writing);
        return Err(error);
    }
    Ok(())
}

fn writing_path(path: &std::path::Path) -> std::io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the IFC path has no file name",
            )
        })?;
    Ok(path.with_file_name(format!(".{file_name}.writing")))
}

fn ifc_element_gone(element_id: &str) -> IonError {
    element_error(
        StatusCode::CONFLICT,
        "IFC_ELEMENT_GONE",
        format!("the IFC no longer contains element {element_id}"),
    )
}

fn refused_property(key: &str, element_id: &str, error: PropertyWriteError) -> IonError {
    match error {
        PropertyWriteError::MissingElement => ifc_element_gone(element_id),
        PropertyWriteError::MissingProperty => element_error(
            StatusCode::BAD_REQUEST,
            "DERIVED_PROPERTY",
            format!("property {key} is derived from the model and has no IFC property to change"),
        ),
        PropertyWriteError::DefinedOnType => element_error(
            StatusCode::BAD_REQUEST,
            "DEFINED_ON_TYPE",
            format!("property {key} is defined on the element's type"),
        ),
        PropertyWriteError::NotASingleValue => element_error(
            StatusCode::BAD_REQUEST,
            "NOT_A_SINGLE_VALUE",
            format!("property {key} is not a single value or a quantity"),
        ),
        PropertyWriteError::KindMismatch => element_error(
            StatusCode::BAD_REQUEST,
            "KIND_MISMATCH",
            format!("property {key} keeps the kind stored in the IFC"),
        ),
        PropertyWriteError::IntegerValue => element_error(
            StatusCode::BAD_REQUEST,
            "INTEGER_VALUE",
            format!("property {key} holds an integer"),
        ),
        PropertyWriteError::ControlCharacter => {
            invalid_parameter("the value contains a control character")
        }
        PropertyWriteError::EmptyNominal => element_error(
            StatusCode::BAD_REQUEST,
            "EMPTY_NOMINAL",
            format!("property {key} has no nominal value in the IFC"),
        ),
        PropertyWriteError::TextTooLong => invalid_parameter(format!(
            "a property value is at most {MAX_TEXT_BYTES} bytes"
        )),
    }
}

fn stored_property_failure(
    id: &str,
    element_id: &str,
    key: &str,
    error: PropertyStoreError,
) -> IonError {
    match error {
        PropertyStoreError::Missing => element_error(
            StatusCode::NOT_FOUND,
            "PROPERTY_NOT_FOUND",
            format!("asset {id} has no property {key} on element {element_id}"),
        ),
        PropertyStoreError::Database(error) => {
            server_failure("the element store could not be updated", error)
        }
        PropertyStoreError::Io(error) => {
            server_failure("the element store could not be updated", error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_SLOTS: Semaphore = Semaphore::const_new(1);

    #[tokio::test]
    async fn a_dropped_request_keeps_its_store_slot_until_its_statement_ends() {
        let asset_dir = tempfile::tempdir().unwrap();
        // sqlite reads a zero-length file as an empty database
        std::fs::File::create(element_store::store_path(asset_dir.path())).unwrap();
        let mut connection = open_store(asset_dir.path())
            .await
            .unwrap()
            .expect("a store");

        let (statement_started, statement_is_running) = tokio::sync::oneshot::channel();
        let (end_statement, statement_may_end) = std::sync::mpsc::channel::<()>();
        let mut statement_started = Some(statement_started);
        // sqlite calls this handler from inside the running statement
        connection
            .lock_handle()
            .await
            .unwrap()
            .set_progress_handler(1, move || {
                if let Some(started) = statement_started.take() {
                    let _ = started.send(());
                    let _ = statement_may_end.recv();
                }
                true
            });
        let store = ReadableStore {
            connection,
            ion_id: 1,
            slot: TEST_SLOTS.acquire().await.unwrap(),
        };

        let request = tokio::spawn(run_past_disconnect(store, |mut connection| async move {
            sqlx::query(
                "WITH RECURSIVE counter(value) AS \
                 (SELECT 1 UNION ALL SELECT value + 1 FROM counter WHERE value < 1000) \
                 SELECT COUNT(*) FROM counter",
            )
            .fetch_one(&mut connection)
            .await
            .map(drop)
            .map_err(store_error)
        }));
        statement_is_running.await.unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());

        assert_eq!(TEST_SLOTS.available_permits(), 0);

        end_statement.send(()).unwrap();
        let _slot_after_the_statement = TEST_SLOTS.acquire().await.unwrap();
    }
}

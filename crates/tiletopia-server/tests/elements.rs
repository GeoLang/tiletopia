mod common;

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{test_state, token};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};
use tiletopia_core::mesh_tiler::MeshData;
use tiletopia_ingest::ifc_elements::ModelOrigin;
use tiletopia_server::db::{JobRecord, JobStatus, ModelPlacement};
use tiletopia_server::element_store::{build_store_and_label_meshes, finish_build, store_path};
use tiletopia_server::{AppState, Asset, AssetStatus, AssetType, router};
use tower::ServiceExt;
use uuid::Uuid;

const OWNER: &str = "element-owner";

const PROJECT: &str = "0project0000000000000a";
const SITE: &str = "0site000000000000000aa";
const BUILDING: &str = "0building0000000000000";
const LEVEL_1: &str = "0level10000000000000aa";
const LEVEL_2: &str = "0level20000000000000aa";
const OUTER_WALL: &str = "0wall$outer0000000000a";
const INNER_WALL: &str = "0wall_inner0000000000a";
const INJECTION_WALL: &str = "0wall0injection000000a";
const DOOR: &str = "0door00000000000000000";
const DOOR_TYPE: &str = "0doortype0000000000000";
const INJECTION_NAME: &str = "x'; DROP TABLE elements; --";

const SMALL_BUILDING_IFC: &str = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [ReferenceView]'),'2;1');
FILE_NAME('small.ifc','2026-11-05',(''),(''),'','','');
FILE_SCHEMA(('IFC4'));
ENDSEC;
DATA;
#1=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);
#3=IFCUNITASSIGNMENT((#1));
#10=IFCPROJECT('0project0000000000000a',$,'Tower',$,$,$,$,$,#3);
#11=IFCSITE('0site000000000000000aa',$,'Plot',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
#12=IFCBUILDING('0building0000000000000',$,'Main',$,$,$,$,$,.ELEMENT.,$,$,$);
#13=IFCBUILDINGSTOREY('0level10000000000000aa',$,'Level 1',$,$,$,$,$,.ELEMENT.,0.);
#14=IFCBUILDINGSTOREY('0level20000000000000aa',$,'Level 2',$,$,$,$,$,.ELEMENT.,3000.);
#20=IFCRELAGGREGATES('0rel000000000000000001',$,$,$,#10,(#11));
#21=IFCRELAGGREGATES('0rel000000000000000002',$,$,$,#11,(#12));
#22=IFCRELAGGREGATES('0rel000000000000000003',$,$,$,#12,(#13,#14));
#30=IFCWALL('0wall$outer0000000000a',$,'Exterior Wall',$,$,$,$,'SENSOR-17',.SOLIDWALL.);
#31=IFCWALL('0wall_inner0000000000a',$,'Interior Wall',$,$,$,$,$,.SOLIDWALL.);
#32=IFCDOOR('0door00000000000000000',$,'Fire Door',$,$,$,$,$,$,$,.DOOR.,.SINGLE_SWING_LEFT.,$);
#34=IFCWALL('0wall0injection000000a',$,'x''; DROP TABLE elements; --',$,$,$,$,$,.SOLIDWALL.);
#33=IFCDOORTYPE('0doortype0000000000000',$,'Steel Door',$,$,(#45),$,$,$,.DOOR.,.SINGLE_SWING_LEFT.,.F.,$);
#40=IFCRELCONTAINEDINSPATIALSTRUCTURE('0rel000000000000000004',$,$,$,(#30,#32),#13);
#41=IFCRELCONTAINEDINSPATIALSTRUCTURE('0rel000000000000000005',$,$,$,(#31,#34),#14);
#42=IFCRELDEFINESBYTYPE('0rel000000000000000006',$,$,$,(#32),#33);
#43=IFCPROPERTYSINGLEVALUE('FireRating',$,IFCLABEL('REI60'),$);
#44=IFCPROPERTYSET('0pset00000000000000001',$,'Pset_WallCommon',$,(#43,#46));
#46=IFCPROPERTYSINGLEVALUE('IsExternal',$,IFCBOOLEAN(.T.),$);
#47=IFCRELDEFINESBYPROPERTIES('0rel000000000000000007',$,$,$,(#30),#44);
#48=IFCQUANTITYLENGTH('Length',$,$,4000.,$);
#49=IFCELEMENTQUANTITY('0qset00000000000000001',$,'Qto_WallBaseQuantities',$,$,(#48));
#50=IFCRELDEFINESBYPROPERTIES('0rel000000000000000008',$,$,$,(#30),#49);
#52=IFCQUANTITYLENGTH('Length',$,$,2500.,$);
#53=IFCELEMENTQUANTITY('0qset00000000000000002',$,'Qto_WallBaseQuantities',$,$,(#52));
#54=IFCRELDEFINESBYPROPERTIES('0rel000000000000000009',$,$,$,(#31),#53);
#45=IFCPROPERTYSET('0pset00000000000000002',$,'Pset_DoorCommon',$,(#51));
#51=IFCPROPERTYSINGLEVALUE('FireRating',$,IFCLABEL('EI30'),$);
#60=IFCMATERIAL('Concrete',$,$);
#61=IFCMATERIALLAYER(#60,200.,$,$,$,$,$);
#62=IFCMATERIALLAYERSET((#61),'Wall 200',$);
#63=IFCMATERIALLAYERSETUSAGE(#62,.AXIS2.,.POSITIVE.,0.,$);
#64=IFCRELASSOCIATESMATERIAL('0rel000000000000000010',$,$,$,(#30),#63);
ENDSEC;
END-ISO-10303-21;
";

const ORIGIN: ModelOrigin = ModelOrigin {
    longitude: 10.0,
    latitude: 20.0,
    height: 0.0,
    x_axis_from_east_radians: 0.0,
};

fn wall_mesh() -> MeshData {
    MeshData {
        positions: vec![[0.0, 0.0, 0.0], [4.0, 0.0, 0.0], [0.0, 3.0, 0.0]],
        normals: Vec::new(),
        texcoords: Vec::new(),
        indices: vec![0, 1, 2],
        name: "Exterior Wall".to_string(),
        asset_id: Some(OUTER_WALL.to_string()),
        class: None,
        storey: None,
        element_color: None,
        base_color_factor: None,
        texture: None,
    }
}

async fn create_asset(state: &AppState, owner: Option<&str>) -> (Uuid, i64) {
    let id = Uuid::new_v4();
    let ion_id = state
        .db
        .create_asset(&Asset {
            id,
            name: "small building".to_string(),
            asset_type: AssetType::Model,
            status: AssetStatus::Ready,
            created_at: chrono::Utc::now(),
            tile_count: 1,
            size_bytes: SMALL_BUILDING_IFC.len() as u64,
            description: String::new(),
            tags: Vec::new(),
            owner_id: owner.map(str::to_string),
        })
        .await
        .unwrap();
    (id, ion_id)
}

async fn build_store(
    asset_dir: std::path::PathBuf,
    ifc: impl AsRef<[u8]> + Send + 'static,
    meshes: Vec<MeshData>,
) -> Vec<MeshData> {
    tokio::task::spawn_blocking(move || {
        let labelled = build_store_and_label_meshes(
            ifc.as_ref(),
            &asset_dir,
            ORIGIN,
            None,
            meshes,
            HashMap::new(),
            None,
        )
        .unwrap();
        finish_build(&asset_dir, true).unwrap();
        labelled
    })
    .await
    .unwrap()
}

async fn small_building(state: &Arc<AppState>) -> i64 {
    let (id, ion_id) = create_asset(state, Some(OWNER)).await;
    build_store(
        state.data_dir.join(id.to_string()),
        SMALL_BUILDING_IFC,
        vec![wall_mesh()],
    )
    .await;
    ion_id
}

async fn get_as(state: &Arc<AppState>, uri: &str, bearer: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::builder().method("GET").uri(uri);
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    let response = router(Arc::clone(state))
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn owner_get(state: &Arc<AppState>, uri: &str) -> Value {
    let (status, body) = get_as(state, uri, Some(&token(OWNER, "viewer"))).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    body
}

fn encoded(text: &str) -> String {
    utf8_percent_encode(text, NON_ALPHANUMERIC).to_string()
}

fn filtered_ids(body: &Value) -> Vec<String> {
    let mut ids: Vec<String> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

async fn ids_matching(state: &Arc<AppState>, ion_id: i64, filter: Value) -> Vec<String> {
    let uri = format!(
        "/v1/assets/{ion_id}/elements?filter={}",
        encoded(&filter.to_string())
    );
    filtered_ids(&owner_get(state, &uri).await)
}

const UPLOADED_IFC: &str = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('uploaded.ifc','2026-11-05',(''),(''),'','','');
FILE_SCHEMA(('IFC2X3'));
ENDSEC;
DATA;
#1=IFCPROJECT('uploadedproject',$,'Uploaded',$,$,$,$,$,$);
#2=IFCSITE('uploadedsite',$,'Plot',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
#3=IFCBUILDING('uploadedbuilding',$,'Main',$,$,$,$,$,.ELEMENT.,$,$,$);
#4=IFCBUILDINGSTOREY('uploadedstorey',$,'Ground',$,$,$,$,$,.ELEMENT.,0.);
#5=IFCRELAGGREGATES('rel1',$,$,$,#1,(#2));
#6=IFCRELAGGREGATES('rel2',$,$,$,#2,(#3));
#7=IFCRELAGGREGATES('rel3',$,$,$,#3,(#4));
#10=IFCCARTESIANPOINT((0.,0.,0.));
#11=IFCAXIS2PLACEMENT3D(#10,$,$);
#12=IFCLOCALPLACEMENT($,#11);
#20=IFCRECTANGLEPROFILEDEF(.AREA.,$,#11,2.0,1.0);
#21=IFCDIRECTION((0.,0.,1.));
#22=IFCEXTRUDEDAREASOLID(#20,#11,#21,3.0);
#30=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#22));
#31=IFCPRODUCTDEFINITIONSHAPE($,$,(#30));
#40=IFCWALL('uploadedwall',$,'Job Wall',$,$,#12,#31,'TAG-9');
#41=IFCOPENINGELEMENT('uploadedopening',$,'Hole',$,$,$,$,$);
#42=IFCRELVOIDSELEMENT('rel4',$,$,$,#40,#41);
#43=IFCRELCONTAINEDINSPATIALSTRUCTURE('rel5',$,$,$,(#40),#4);
ENDSEC;
END-ISO-10303-21;
";

async fn settle_newest_job(state: &Arc<AppState>, asset_id: Uuid) -> JobRecord {
    let worker = Arc::clone(&state.job_queue).start().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let job = loop {
        let jobs = state.db.list_jobs_for_asset(asset_id).await.unwrap();
        let current = jobs.into_iter().next().expect("a job for the asset");
        if matches!(current.status, JobStatus::Done | JobStatus::Failed) {
            break current;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the job never settled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    worker.abort();
    job
}

async fn upload_and_tile(state: &Arc<AppState>) -> (Uuid, i64) {
    let boundary = "elementtestboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"longitude\"\r\n\r\n10\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"latitude\"\r\n\r\n20\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"uploaded.ifc\"\r\n\r\n\
         {UPLOADED_IFC}\r\n--{boundary}--\r\n"
    );
    let response = router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/assets")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header(
                    "authorization",
                    format!("Bearer {}", token(OWNER, "editor")),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let uploaded: Value = serde_json::from_slice(&bytes).unwrap();
    let id = Uuid::parse_str(uploaded["id"].as_str().unwrap()).unwrap();

    let job = settle_newest_job(state, id).await;
    assert_eq!(job.status, JobStatus::Done, "error: {:?}", job.error);

    let (_, ion_id) = state.db.get_asset_with_ion_id(id).await.unwrap().unwrap();
    (id, ion_id)
}

#[tokio::test]
async fn an_uploaded_ifc_reaches_the_routes_through_the_tiling_job() {
    let state = test_state().await;
    let (id, ion_id) = upload_and_tile(&state).await;
    assert!(store_path(&state.data_dir.join(id.to_string())).is_file());

    let index = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/property-index"),
    )
    .await;
    assert_eq!(index["status"], "Completed");

    let wall = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/uploadedwall"),
    )
    .await;
    assert_eq!(wall["ifcClass"], "IfcWall");
    assert_eq!(wall["storey"], "Ground");
    assert_eq!(wall["properties"]["Element.Tag"]["value"], "TAG-9");
    let center = &wall["location"]["center"];
    assert!(
        (center["longitude"].as_f64().unwrap() - 10.0).abs() < 1e-3,
        "{wall}"
    );
    assert!(
        (center["latitude"].as_f64().unwrap() - 20.0).abs() < 1e-3,
        "{wall}"
    );
    assert!(wall["location"]["radius"].as_f64().unwrap() > 0.0, "{wall}");

    let opening = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/uploadedopening"),
    )
    .await;
    assert_eq!(opening["parentId"], "uploadedwall");
    assert!(opening.get("location").is_none(), "{opening}");

    let storey = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/uploadedstorey"),
    )
    .await;
    assert_eq!(storey["parentId"], "uploadedbuilding");
    assert!(storey.get("location").is_none(), "{storey}");
    let base_height = storey["baseHeight"]
        .as_f64()
        .expect("an IFC2X3 Elevation of 0");
    assert!(base_height.abs() < 1e-3, "{storey}");

    let path = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/tree/parents?elementId=uploadedopening"),
    )
    .await;
    assert_eq!(path["path"].as_array().unwrap().len(), 6);

    let found = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/search?search=TAG-9"),
    )
    .await;
    assert_eq!(found["items"][0]["id"], "uploadedwall");

    let (status, _) = get_as(&state, &format!("/v1/assets/{ion_id}/elements"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_tree_walks_from_the_project_to_a_wall() {
    let state = test_state().await;
    let ion_id = small_building(&state).await;

    let roots = owner_get(&state, &format!("/v1/assets/{ion_id}/tree/children")).await;
    assert_eq!(roots["nodes"].as_array().unwrap().len(), 1);
    let root = &roots["nodes"][0];
    assert_eq!(root["id"], PROJECT);
    assert_eq!(root["kind"], "subject");
    assert_eq!(root["ifcClass"], "IfcProject");
    assert_eq!(root["hasChildren"], true);

    let mut key = root["key"].as_str().unwrap().to_string();
    for (expected_id, expected_kind) in
        [(SITE, "subject"), (BUILDING, "subject"), (LEVEL_1, "model")]
    {
        let uri = format!(
            "/v1/assets/{ion_id}/tree/children?nodeKey={}",
            encoded(&key)
        );
        let nodes = owner_get(&state, &uri).await;
        let node = nodes["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == expected_id)
            .unwrap_or_else(|| panic!("no {expected_id} in {nodes}"));
        assert_eq!(node["kind"], expected_kind);
        key = node["key"].as_str().unwrap().to_string();
    }

    let storey_children = owner_get(
        &state,
        &format!(
            "/v1/assets/{ion_id}/tree/children?nodeKey={}",
            encoded(&key)
        ),
    )
    .await;
    let child_ids: Vec<&str> = storey_children["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].as_str().unwrap())
        .collect();
    assert_eq!(child_ids, [OUTER_WALL, DOOR]);
    assert_eq!(storey_children["nodes"][0]["kind"], "element");
    assert_eq!(storey_children["nodes"][0]["hasChildren"], false);

    let path = owner_get(
        &state,
        &format!(
            "/v1/assets/{ion_id}/tree/parents?elementId={}",
            encoded(OUTER_WALL)
        ),
    )
    .await;
    let path_ids: Vec<&str> = path["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(path_ids, [PROJECT, SITE, BUILDING, LEVEL_1, OUTER_WALL]);
    assert_eq!(path["path"][3]["kind"], "model");
    assert_eq!(path["path"][4]["kind"], "element");
}

#[tokio::test]
async fn storeys_report_their_elevation_as_ellipsoid_height() {
    let state = test_state().await;
    let ion_id = small_building(&state).await;

    let storeys = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/tree/children?nodeKey={BUILDING}"),
    )
    .await;
    let base_height = |id: &str| {
        storeys["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == id)
            .unwrap_or_else(|| panic!("no {id} in {storeys}"))["baseHeight"]
            .as_f64()
            .unwrap_or_else(|| panic!("{id} has no baseHeight in {storeys}"))
    };
    assert!(
        (base_height(LEVEL_1) - ORIGIN.height).abs() < 1e-3,
        "{storeys}"
    );
    assert!(
        (base_height(LEVEL_2) - (ORIGIN.height + 3.0)).abs() < 1e-3,
        "3000 mm above the origin: {storeys}"
    );

    let buildings = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/tree/children?nodeKey={SITE}"),
    )
    .await;
    assert!(
        buildings["nodes"][0].get("baseHeight").is_none(),
        "{buildings}"
    );

    let storey = owner_get(&state, &format!("/v1/assets/{ion_id}/elements/{LEVEL_2}")).await;
    let height = storey["baseHeight"].as_f64().unwrap();
    assert!((height - (ORIGIN.height + 3.0)).abs() < 1e-3, "{storey}");

    let wall = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/{}", encoded(OUTER_WALL)),
    )
    .await;
    assert!(wall.get("baseHeight").is_none(), "{wall}");
}

#[tokio::test]
async fn an_unknown_tree_node_or_element_is_refused() {
    let state = test_state().await;
    let ion_id = small_building(&state).await;
    let bearer = token(OWNER, "viewer");

    let (status, body) = get_as(
        &state,
        &format!("/v1/assets/{ion_id}/tree/children?nodeKey=nope"),
        Some(&bearer),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "INVALID_PARAMETER");

    let (status, body) = get_as(
        &state,
        &format!("/v1/assets/{ion_id}/tree/parents?elementId=nope"),
        Some(&bearer),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "ELEMENT_NOT_FOUND");

    let (status, body) = get_as(
        &state,
        &format!("/v1/assets/{ion_id}/elements/nope"),
        Some(&bearer),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "ELEMENT_NOT_FOUND");
}

#[tokio::test]
async fn an_element_reads_back_psets_quantities_types_and_materials() {
    let state = test_state().await;
    let ion_id = small_building(&state).await;

    let wall = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/{}", encoded(OUTER_WALL)),
    )
    .await;
    assert_eq!(wall["id"], OUTER_WALL);
    assert_eq!(wall["label"], "Exterior Wall");
    assert_eq!(wall["ifcClass"], "IfcWall");
    assert_eq!(wall["storey"], "Level 1");
    assert_eq!(wall["parentId"], LEVEL_1);
    assert_eq!(wall["location"]["radius"], 2.5);
    assert!(
        (wall["location"]["center"]["latitude"].as_f64().unwrap() - 20.0).abs() < 1e-3,
        "{wall}"
    );

    let properties = &wall["properties"];
    assert_eq!(
        properties["Pset_WallCommon.FireRating"],
        json!({ "displayValue": "REI60", "value": "REI60", "units": null })
    );
    assert_eq!(properties["Pset_WallCommon.IsExternal"]["value"], true);
    assert_eq!(
        properties["Qto_WallBaseQuantities.Length"],
        json!({ "displayValue": "4000 mm", "value": 4000.0, "units": "mm" })
    );
    assert_eq!(properties["Material[0].Name"]["value"], "Concrete");
    assert_eq!(properties["Material[0].Set"]["value"], "Wall 200");
    assert_eq!(properties["Material[0].Thickness"]["units"], "mm");
    assert_eq!(properties["Spatial.Storey"]["value"], "Level 1");
    assert_eq!(properties["Element.Tag"]["value"], "SENSOR-17");

    let door = owner_get(&state, &format!("/v1/assets/{ion_id}/elements/{DOOR}")).await;
    assert_eq!(door["properties"]["Type.Name"]["value"], "Steel Door");
    assert_eq!(door["properties"]["Type.Class"]["value"], "IfcDoorType");
    assert_eq!(door["properties"]["Type.GlobalId"]["value"], DOOR_TYPE);
    assert_eq!(
        door["properties"]["Pset_DoorCommon.FireRating"]["value"], "EI30",
        "a door inherits its type's property set"
    );

    let storey = owner_get(&state, &format!("/v1/assets/{ion_id}/elements/{LEVEL_2}")).await;
    assert_eq!(storey["ifcClass"], "IfcBuildingStorey");
    assert!(
        storey["properties"].get("Element.Tag").is_none(),
        "{storey}"
    );
    assert!(storey.get("location").is_none(), "{storey}");
}

#[tokio::test]
async fn filters_match_numbers_patterns_and_root_fields() {
    let state = test_state().await;
    let ion_id = small_building(&state).await;

    assert_eq!(
        ids_matching(
            &state,
            ion_id,
            json!({ "Qto_WallBaseQuantities.Length": { "$gt": 3000 } })
        )
        .await,
        [OUTER_WALL]
    );
    assert_eq!(
        ids_matching(
            &state,
            ion_id,
            json!({ "Qto_WallBaseQuantities.Length": { "$gte": 2500, "$lt": 3000 } })
        )
        .await,
        [INNER_WALL]
    );
    assert_eq!(
        ids_matching(
            &state,
            ion_id,
            json!({ "Pset_WallCommon.FireRating": { "$like": "%ei6%" } })
        )
        .await,
        [OUTER_WALL]
    );
    assert_eq!(
        ids_matching(
            &state,
            ion_id,
            json!({ "$or": [{ "@label": "Fire Door" }, { "@label": { "$like": "exterior%" } }] })
        )
        .await,
        [DOOR, OUTER_WALL]
    );
    assert_eq!(
        ids_matching(
            &state,
            ion_id,
            json!({ "@classFullName": "IfcWall", "@storey": "Level 1" })
        )
        .await,
        [OUTER_WALL]
    );
    assert_eq!(
        ids_matching(&state, ion_id, json!({ "Element.Tag": "SENSOR-17" })).await,
        [OUTER_WALL]
    );
    assert_eq!(
        ids_matching(
            &state,
            ion_id,
            json!({ "Pset_WallCommon.IsExternal": true })
        )
        .await,
        [OUTER_WALL]
    );
    assert!(
        ids_matching(
            &state,
            ion_id,
            json!({ "Qto_WallBaseQuantities.Length": "4000" })
        )
        .await
        .is_empty(),
        "a string operand does not match a number"
    );
    assert_eq!(
        ids_matching(
            &state,
            ion_id,
            json!({ "@classFullName": "IfcWall", "Pset_WallCommon.FireRating": null })
        )
        .await
        .len(),
        2,
        "$eq null matches walls with no such property"
    );
    assert_eq!(
        ids_matching(
            &state,
            ion_id,
            json!({ "Pset_WallCommon.FireRating": { "$ne": "EI30" } })
        )
        .await,
        [OUTER_WALL],
        "$ne does not match elements without the property"
    );
}

#[tokio::test]
async fn sql_injection_strings_stay_data() {
    let state = test_state().await;
    let ion_id = small_building(&state).await;

    assert_eq!(
        ids_matching(&state, ion_id, json!({ "@label": INJECTION_NAME })).await,
        [INJECTION_WALL]
    );
    for hostile in [
        json!({ "@label": "' OR '1'='1" }),
        json!({ "@label": { "$like": "' OR 1=1; DROP TABLE elements; --" } }),
        json!({ "' OR 1=1; DROP TABLE properties; --": "x" }),
        json!({ "Pset_WallCommon.FireRating": { "$in": ["'); DROP TABLE properties; --"] } }),
    ] {
        assert!(
            ids_matching(&state, ion_id, hostile.clone())
                .await
                .is_empty(),
            "{hostile}"
        );
    }
    let all = owner_get(&state, &format!("/v1/assets/{ion_id}/elements")).await;
    assert_eq!(all["total"], 9, "the tables are still there");
    let wall = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/{}", encoded(OUTER_WALL)),
    )
    .await;
    assert_eq!(
        wall["properties"]["Pset_WallCommon.FireRating"]["value"],
        "REI60"
    );

    let hostile_id = encoded("' OR '1'='1");
    let (status, _) = get_as(
        &state,
        &format!("/v1/assets/{ion_id}/elements/{hostile_id}"),
        Some(&token(OWNER, "viewer")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bad_filters_are_refused_by_name() {
    let state = test_state().await;
    let ion_id = small_building(&state).await;

    let uri = format!(
        "/v1/assets/{ion_id}/elements?filter={}",
        encoded(r#"{"A.B":{"$regex":"x"}}"#)
    );
    let (status, body) = get_as(&state, &uri, Some(&token(OWNER, "viewer"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "INVALID_FILTER");
    assert!(
        body["message"].as_str().unwrap().contains("$regex"),
        "{body}"
    );

    let (status, body) = get_as(
        &state,
        &format!("/v1/assets/{ion_id}/elements?top=1001"),
        Some(&token(OWNER, "viewer")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "INVALID_PARAMETER");
}

#[tokio::test]
async fn pages_follow_the_continuation_token_in_file_order() {
    let state = test_state().await;
    let ion_id = small_building(&state).await;

    let first = owner_get(&state, &format!("/v1/assets/{ion_id}/elements?top=4")).await;
    assert_eq!(first["total"], 9);
    assert_eq!(first["items"].as_array().unwrap().len(), 4);
    assert_eq!(first["items"][0]["id"], PROJECT);
    let token_value = first["continuationToken"].as_str().unwrap();

    let second = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements?top=5&continuationToken={token_value}"),
    )
    .await;
    assert_eq!(second["items"].as_array().unwrap().len(), 5);
    assert!(second.get("continuationToken").is_none(), "{second}");

    let (status, body) = get_as(
        &state,
        &format!("/v1/assets/{ion_id}/elements?continuationToken=1;DROP"),
        Some(&token(OWNER, "viewer")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn search_covers_id_name_class_and_property_values() {
    let state = test_state().await;
    let ion_id = small_building(&state).await;
    let found = |search: &'static str| {
        let state = Arc::clone(&state);
        async move {
            let uri = format!(
                "/v1/assets/{ion_id}/elements/search?search={}",
                encoded(search)
            );
            filtered_ids(&owner_get(&state, &uri).await)
        }
    };

    assert_eq!(found("REI60").await, [OUTER_WALL], "a property value");
    assert_eq!(found("exterior").await, [OUTER_WALL], "a name");
    assert_eq!(found("IfcDoor").await, [DOOR], "a class");
    assert_eq!(found(DOOR).await, [DOOR], "an id");
    assert_eq!(found("exteri*").await, [OUTER_WALL], "a prefix");
    assert_eq!(found("rei + -missing").await, [OUTER_WALL]);
    assert!(found("rei + -exterior").await.is_empty());
    assert_eq!(found("SENSOR-17").await, [OUTER_WALL], "a tag");
    assert_eq!(found("\"fire door\"").await, [DOOR], "a phrase");
    assert_eq!(found("wall + exterior").await, [OUTER_WALL]);
    assert_eq!(
        found("REI 60").await,
        [OUTER_WALL],
        "the digit split of REI60"
    );
    assert!(found("name:exterior OR NEAR(a b)").await.is_empty());

    let hit = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/search?search=exterior"),
    )
    .await;
    assert_eq!(hit["total"], 1);
    assert_eq!(hit["items"][0]["properties"]["className"], "IfcWall");
    assert_eq!(hit["items"][0]["properties"]["userLabel"], "Exterior Wall");

    let (status, body) = get_as(
        &state,
        &format!("/v1/assets/{ion_id}/elements/search?search=-wall"),
        Some(&token(OWNER, "viewer")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "INVALID_SEARCH");
}

const MANY_WALLS: usize = 60;

#[tokio::test]
async fn search_total_counts_every_match_and_not_the_page() {
    let state = test_state().await;
    let (id, ion_id) = create_asset(&state, Some(OWNER)).await;
    let walls: String = (0..MANY_WALLS)
        .map(|index| {
            format!(
                "#{}=IFCWALL('manywall{index:014}',$,'Wall {index}',$,$,$,$,$,$);\n",
                index + 2
            )
        })
        .collect();
    let ifc = format!(
        "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION((''),'2;1');\n\
         FILE_NAME('many.ifc','2026-11-05',(''),(''),'','','');\nFILE_SCHEMA(('IFC4'));\nENDSEC;\n\
         DATA;\n#1=IFCPROJECT('manyproject',$,'P',$,$,$,$,$,$);\n{walls}ENDSEC;\nEND-ISO-10303-21;\n"
    );
    build_store(state.data_dir.join(id.to_string()), ifc, Vec::new()).await;

    let first = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/search?search=wall"),
    )
    .await;
    assert_eq!(first["total"], MANY_WALLS);
    let first_page = first["items"].as_array().unwrap().len();
    assert!(first_page < MANY_WALLS, "{first_page} hits on one page");
    let token_value = first["continuationToken"].as_str().unwrap();

    let second = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/search?search=wall&continuationToken={token_value}"),
    )
    .await;
    assert_eq!(second["total"], MANY_WALLS);
    assert_eq!(
        first_page + second["items"].as_array().unwrap().len(),
        MANY_WALLS
    );
}

#[tokio::test]
async fn aggregate_answers_ions_shape_and_group_by() {
    let state = test_state().await;
    let ion_id = small_building(&state).await;
    let length = "Qto_WallBaseQuantities.Length";

    let plain = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/aggregate?aggregations={length},Pset_WallCommon.FireRating"),
    )
    .await;
    assert_eq!(plain["total"], 9);
    assert_eq!(
        plain["aggregations"][0],
        json!({
            "propertyKey": length, "count": 2, "min": 2500.0, "max": 4000.0, "avg": 3250.0, "sum": 6500.0, "units": ["mm"],
        })
    );
    assert_eq!(
        plain["aggregations"][1],
        json!({
            "propertyKey": "Pset_WallCommon.FireRating", "count": 0, "min": null, "max": null, "avg": null, "sum": 0.0, "units": [],
        }),
        "a text property answers with no numeric values"
    );
    assert!(plain.get("groups").is_none());

    let filter = encoded(r#"{"@classFullName":"IfcWall"}"#);
    let grouped = owner_get(
        &state,
        &format!(
            "/v1/assets/{ion_id}/elements/aggregate?aggregations={length}&groupBy=@storey&filter={filter}"
        ),
    )
    .await;
    assert_eq!(grouped["total"], 3);
    assert_eq!(grouped["aggregations"][0]["count"], 2);
    let groups = grouped["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["value"], "Level 1");
    assert_eq!(groups[0]["total"], 1);
    assert_eq!(groups[0]["aggregations"][0]["sum"], 4000.0);
    assert_eq!(groups[0]["aggregations"][0]["units"], json!(["mm"]));
    assert_eq!(groups[1]["value"], "Level 2");
    assert_eq!(groups[1]["total"], 2);
    assert_eq!(groups[1]["aggregations"][0]["count"], 1);

    let counted = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/aggregate?groupBy=@classFullName"),
    )
    .await;
    let wall_group = counted["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["value"] == "IfcWall")
        .unwrap();
    assert_eq!(wall_group["total"], 3);

    let by_property = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/aggregate?groupBy=Pset_WallCommon.FireRating"),
    )
    .await;
    assert_eq!(by_property["groups"][0]["value"], Value::Null);
    assert_eq!(by_property["groups"][0]["total"], 8);
    assert_eq!(by_property["groups"][1]["value"], "REI60");

    let (status, body) = get_as(
        &state,
        &format!("/v1/assets/{ion_id}/elements/aggregate"),
        Some(&token(OWNER, "viewer")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn aggregate_names_every_unit_a_sum_mixes() {
    let state = test_state().await;
    let (id, ion_id) = create_asset(&state, Some(OWNER)).await;
    const MIXED_UNITS: &str = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION((''),'2;1');
FILE_NAME('m.ifc','2026-11-05',(''),(''),'','','');
FILE_SCHEMA(('IFC4'));
ENDSEC;
DATA;
#1=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);
#2=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);
#3=IFCUNITASSIGNMENT((#1));
#4=IFCPROJECT('mixedproject',$,'P',$,$,$,$,$,#3);
#5=IFCWALL('wallmillimetres',$,'A',$,$,$,$,$,$);
#6=IFCWALL('wallmetres',$,'B',$,$,$,$,$,$);
#7=IFCQUANTITYLENGTH('Length',$,$,4000.,$);
#8=IFCQUANTITYLENGTH('Length',$,#2,4.,$);
#9=IFCELEMENTQUANTITY('qsetmillimetres',$,'Q',$,$,(#7));
#10=IFCELEMENTQUANTITY('qsetmetres',$,'Q',$,$,(#8));
#11=IFCRELDEFINESBYPROPERTIES('rel1',$,$,$,(#5),#9);
#12=IFCRELDEFINESBYPROPERTIES('rel2',$,$,$,(#6),#10);
ENDSEC;
END-ISO-10303-21;
";
    build_store(state.data_dir.join(id.to_string()), MIXED_UNITS, Vec::new()).await;

    let answer = owner_get(
        &state,
        &format!(
            "/v1/assets/{ion_id}/elements/aggregate?aggregations=Q.Length&groupBy=@classFullName"
        ),
    )
    .await;
    assert_eq!(answer["aggregations"][0]["units"], json!(["m", "mm"]));
    let wall_group = answer["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["value"] == "IfcWall")
        .unwrap();
    assert_eq!(wall_group["aggregations"][0]["units"], json!(["m", "mm"]));
}

#[tokio::test]
async fn properties_list_names_and_types_with_a_next_link() {
    let state = test_state().await;
    let (id, ion_id) = create_asset(&state, Some(OWNER)).await;
    build_store(
        state.data_dir.join(id.to_string()),
        SMALL_BUILDING_IFC,
        Vec::new(),
    )
    .await;

    let all = owner_get(&state, &format!("/v1/assets/{ion_id}/elements/properties")).await;
    let items = all["items"].as_array().unwrap();
    assert_eq!(all["total"].as_u64().unwrap() as usize, items.len());
    assert!(items.contains(&json!({ "name": "Qto_WallBaseQuantities.Length", "type": "number" })));
    assert!(items.contains(&json!({ "name": "Pset_WallCommon.FireRating", "type": "string" })));
    assert!(items.contains(&json!({ "name": "Pset_WallCommon.IsExternal", "type": "boolean" })));

    let response = router(Arc::clone(&state))
        .oneshot(
            Request::builder()
                .uri(format!("/v1/assets/{ion_id}/elements/properties?limit=1"))
                .header(
                    "authorization",
                    format!("Bearer {}", token(OWNER, "viewer")),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let link = response.headers()["link"].to_str().unwrap();
    assert!(link.ends_with("rel=\"next\""), "{link}");
    assert!(link.contains("limit=1&page=2"), "{link}");
}

#[tokio::test]
async fn property_index_answers_completed_with_named_warnings() {
    let state = test_state().await;
    let (id, ion_id) = create_asset(&state, Some(OWNER)).await;
    const WITH_DUPLICATE: &str = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION((''),'2;1');
FILE_NAME('d.ifc','2026-11-05',(''),(''),'','','');
FILE_SCHEMA(('IFC4'));
ENDSEC;
DATA;
#1=IFCPROJECT('duplicateproject',$,'P',$,$,$,$,$,$);
#2=IFCWALL('same',$,'A',$,$,$,$,$,$);
#3=IFCWALL('same',$,'B',$,$,$,$,$,$);
ENDSEC;
END-ISO-10303-21;
";
    build_store(
        state.data_dir.join(id.to_string()),
        WITH_DUPLICATE,
        Vec::new(),
    )
    .await;

    let status = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/property-index"),
    )
    .await;
    assert_eq!(status["status"], "Completed");
    assert!(status["requestedAt"].is_string() && status["updatedAt"].is_string());
    let warnings = status["warnings"].as_array().unwrap();
    assert!(warnings.contains(&json!({ "name": "duplicate_global_id", "count": 1 })));
    assert!(warnings.contains(&json!({ "name": "uncontained_element", "count": 1 })));
}

#[tokio::test]
async fn meshes_get_the_class_and_storey_their_element_is_stored_under() {
    let state = test_state().await;
    let (id, ion_id) = create_asset(&state, Some(OWNER)).await;
    let unknown = MeshData {
        asset_id: Some("not-in-the-file".to_string()),
        ..wall_mesh()
    };
    let labelled = build_store(
        state.data_dir.join(id.to_string()),
        SMALL_BUILDING_IFC,
        vec![wall_mesh(), unknown],
    )
    .await;
    assert_eq!(labelled[0].class.as_deref(), Some("IfcWall"));
    assert_eq!(labelled[0].storey.as_deref(), Some("Level 1"));
    assert_eq!(labelled[1].class, None);
    assert_eq!(labelled[1].storey, None);

    let wall = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/{}", encoded(OUTER_WALL)),
    )
    .await;
    assert_eq!(wall["ifcClass"], "IfcWall");
    assert_eq!(wall["properties"]["Spatial.Storey"]["value"], "Level 1");
}

#[tokio::test]
async fn access_needs_a_token_and_the_listing_read_rule() {
    let state = test_state().await;
    let ion_id = small_building(&state).await;
    let (no_owner_id, no_owner_ion_id) = create_asset(&state, None).await;
    build_store(
        state.data_dir.join(no_owner_id.to_string()),
        SMALL_BUILDING_IFC,
        Vec::new(),
    )
    .await;
    let missing = 999_999;

    let routes = [
        "elements".to_string(),
        format!("elements/{}", encoded(OUTER_WALL)),
        "elements/search?search=wall".to_string(),
        "elements/aggregate?groupBy=@storey".to_string(),
        "elements/properties".to_string(),
        "elements/property-index".to_string(),
        "tree/children".to_string(),
        format!("tree/parents?elementId={}", encoded(OUTER_WALL)),
    ];
    let stranger = token("someone-else", "viewer");
    let (_, missing_body) = get_as(
        &state,
        &format!("/v1/assets/{missing}/elements"),
        Some(&stranger),
    )
    .await;
    for route in &routes {
        let uri = format!("/v1/assets/{ion_id}/{route}");

        let (status, _) = get_as(&state, &uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "anonymous {uri}");

        let (status, _) = get_as(
            &state,
            &format!("{uri}{}", separator(&uri, "access_token=x")),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "a query token {uri}");

        let (status, _) = get_as(&state, &uri, Some("not-a-jwt")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "a bad token {uri}");

        let (status, body) = get_as(&state, &uri, Some(&stranger)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "another owner {uri}");
        assert_eq!(body["code"], "ResourceNotFound");
        assert_eq!(
            body["message"]
                .as_str()
                .unwrap()
                .replace(&ion_id.to_string(), ""),
            missing_body["message"]
                .as_str()
                .unwrap()
                .replace(&missing.to_string(), ""),
            "a hidden asset reads the same as a missing one"
        );

        owner_get(&state, &uri).await;

        let (status, _) = get_as(&state, &uri, Some(&token("an-admin", "admin"))).await;
        assert_eq!(status, StatusCode::OK, "an admin {uri}");

        let no_owner_uri = format!("/v1/assets/{no_owner_ion_id}/{route}");
        let (status, _) = get_as(&state, &no_owner_uri, Some(&stranger)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "no owner {no_owner_uri}");
        let (status, _) = get_as(&state, &no_owner_uri, Some(&token("an-admin", "admin"))).await;
        assert_eq!(status, StatusCode::OK, "an admin, no owner {no_owner_uri}");
    }
}

fn separator(uri: &str, parameter: &str) -> String {
    if uri.contains('?') {
        format!("&{parameter}")
    } else {
        format!("?{parameter}")
    }
}

#[tokio::test]
async fn path_tricks_do_not_reach_elements_anonymously() {
    let state = test_state().await;
    let ion_id = small_building(&state).await;

    for uri in [
        format!("/v1/assets/{ion_id}/elements/"),
        format!("/v1//assets/{ion_id}/elements"),
        format!("/v1/assets/{ion_id}//elements"),
        format!("/v1/assets/{ion_id}/%65lements"),
        format!("/v1/assets/{ion_id}%2Felements"),
        format!("/v1/assets/{ion_id}/ELEMENTS"),
        format!("/v1/assets/{ion_id}/tree/children/"),
        format!("/v1/assets/{ion_id}/tree%2Fchildren"),
    ] {
        let (status, body) = get_as(&state, &uri, None).await;
        assert!(
            status == StatusCode::UNAUTHORIZED || status == StatusCode::NOT_FOUND,
            "{uri}: {status}"
        );
        assert!(
            body.get("items").is_none() && body.get("nodes").is_none(),
            "{uri}: {body}"
        );
    }
}

#[tokio::test]
async fn a_non_ifc_asset_answers_404_naming_the_store() {
    let state = test_state().await;
    let (_, ion_id) = create_asset(&state, Some(OWNER)).await;

    let (status, body) = get_as(
        &state,
        &format!("/v1/assets/{ion_id}/elements"),
        Some(&token(OWNER, "viewer")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "ASSET_HAS_NO_ELEMENT_STORE");
    assert!(body["message"].as_str().unwrap().contains("IFC"), "{body}");
}

#[tokio::test]
async fn deleting_the_asset_removes_the_store_file() {
    let state = test_state().await;
    let (id, ion_id) = create_asset(&state, Some(OWNER)).await;
    let asset_dir = state.data_dir.join(id.to_string());
    build_store(asset_dir.clone(), SMALL_BUILDING_IFC, Vec::new()).await;
    assert!(store_path(&asset_dir).is_file());

    let response = router(Arc::clone(&state))
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/assets/{id}"))
                .header(
                    "authorization",
                    format!("Bearer {}", token(OWNER, "editor")),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(!store_path(&asset_dir).exists());

    let (status, body) = get_as(
        &state,
        &format!("/v1/assets/{ion_id}/elements"),
        Some(&token(OWNER, "viewer")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "ResourceNotFound");
}

async fn retile(state: &Arc<AppState>, asset_id: Uuid) -> JobRecord {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/assets/{asset_id}/tile"))
                .header(
                    "authorization",
                    format!("Bearer {}", token(OWNER, "editor")),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    settle_newest_job(state, asset_id).await
}

fn element_store_files(asset_dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(asset_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("elements.sqlite"))
        .collect()
}

#[tokio::test]
async fn a_rebuild_replaces_the_store_and_a_failed_tiling_keeps_nothing() {
    let state = test_state().await;
    let (id, ion_id) = upload_and_tile(&state).await;
    let asset_dir = state.data_dir.join(id.to_string());
    let before = owner_get(&state, &format!("/v1/assets/{ion_id}/elements")).await;
    assert!(before["total"].as_u64().unwrap() > 0, "{before}");

    // a file where the tiles folder belongs fails the job after its element store is built
    std::fs::remove_dir_all(asset_dir.join("tiles")).unwrap();
    std::fs::write(asset_dir.join("tiles"), b"in the way").unwrap();
    let failed = retile(&state, id).await;
    assert_eq!(failed.status, JobStatus::Failed);
    assert_eq!(
        element_store_files(&asset_dir),
        Vec::<String>::new(),
        "a failed job publishes no store and keeps no old one"
    );
    let (status, body) = get_as(
        &state,
        &format!("/v1/assets/{ion_id}/elements"),
        Some(&token(OWNER, "viewer")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "ASSET_HAS_NO_ELEMENT_STORE");

    std::fs::remove_file(asset_dir.join("tiles")).unwrap();
    let rebuilt = retile(&state, id).await;
    assert_eq!(
        rebuilt.status,
        JobStatus::Done,
        "error: {:?}",
        rebuilt.error
    );
    let after = owner_get(&state, &format!("/v1/assets/{ion_id}/elements")).await;
    assert_eq!(after["total"], before["total"]);
}

async fn ifc_with_store(state: &Arc<AppState>) -> (Uuid, i64, std::path::PathBuf) {
    let (id, ion_id) = create_asset(state, Some(OWNER)).await;
    let asset_dir = state.data_dir.join(id.to_string());
    build_store(asset_dir.clone(), SMALL_BUILDING_IFC, vec![wall_mesh()]).await;
    std::fs::create_dir_all(asset_dir.join("input")).unwrap();
    let ifc_path = asset_dir.join("input/small.ifc");
    std::fs::write(&ifc_path, SMALL_BUILDING_IFC).unwrap();
    state
        .db
        .create_job(&JobRecord {
            id: Uuid::new_v4(),
            asset_id: id,
            status: JobStatus::Done,
            progress: 1.0,
            input_path: "input/small.ifc".to_string(),
            output_format: "3dtiles".to_string(),
            created_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            error: None,
            points_processed: 0,
            tiles_written: 0,
            placement: ModelPlacement::default(),
            mesh_compression: Default::default(),
            texture_compression: Default::default(),
            photogrammetry_preset: Default::default(),
        })
        .await
        .unwrap();
    (id, ion_id, ifc_path)
}

async fn put_property(
    state: &Arc<AppState>,
    ion_id: i64,
    element_id: &str,
    key: &str,
    value: Value,
    subject: &str,
    role: &str,
) -> (StatusCode, Vec<u8>) {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!(
                    "/v1/assets/{ion_id}/elements/{}/properties",
                    encoded(element_id)
                ))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", token(subject, role)))
                .body(Body::from(
                    json!({ "key": key, "value": value }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}

fn json_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

#[tokio::test]
async fn an_editor_writes_a_property_into_the_ifc_and_the_store() {
    let state = test_state().await;
    let (_id, ion_id, ifc_path) = ifc_with_store(&state).await;
    let original = std::fs::read(&ifc_path).unwrap();

    let (status, bytes) = put_property(
        &state,
        ion_id,
        OUTER_WALL,
        "Pset_WallCommon.FireRating",
        json!(1),
        OWNER,
        "editor",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", json_body(&bytes));
    assert_eq!(json_body(&bytes)["code"], "KIND_MISMATCH");
    assert_eq!(std::fs::read(&ifc_path).unwrap(), original);

    let (status, bytes) = put_property(
        &state,
        ion_id,
        DOOR,
        "Pset_DoorCommon.FireRating",
        json!("EI90"),
        OWNER,
        "editor",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", json_body(&bytes));
    assert_eq!(json_body(&bytes)["code"], "DEFINED_ON_TYPE");
    assert!(
        json_body(&bytes)["message"]
            .as_str()
            .unwrap()
            .contains("defined on the element's type"),
        "{}",
        json_body(&bytes)
    );
    assert_eq!(std::fs::read(&ifc_path).unwrap(), original);

    let (status, bytes) = put_property(
        &state,
        ion_id,
        OUTER_WALL,
        "Element.Tag",
        json!("other"),
        OWNER,
        "editor",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", json_body(&bytes));
    assert_eq!(json_body(&bytes)["code"], "DERIVED_PROPERTY");
    assert_eq!(std::fs::read(&ifc_path).unwrap(), original);

    let (status, bytes) = put_property(
        &state,
        ion_id,
        OUTER_WALL,
        "Pset_WallCommon.FireRating",
        json!("REI90"),
        OWNER,
        "viewer",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(bytes.is_empty(), "{bytes:?}");
    assert_eq!(std::fs::read(&ifc_path).unwrap(), original);

    let (status, bytes) = put_property(
        &state,
        ion_id,
        OUTER_WALL,
        "Pset_WallCommon.FireRating",
        json!("REI90"),
        "someone-else",
        "editor",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", json_body(&bytes));
    assert_eq!(json_body(&bytes)["code"], "ResourceNotFound");
    assert_eq!(std::fs::read(&ifc_path).unwrap(), original);

    let (status, bytes) = put_property(
        &state,
        ion_id,
        OUTER_WALL,
        "missing",
        json!("x"),
        OWNER,
        "editor",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", json_body(&bytes));
    assert_eq!(json_body(&bytes)["code"], "PROPERTY_NOT_FOUND");

    let (status, bytes) = put_property(
        &state,
        ion_id,
        OUTER_WALL,
        "Qto_WallBaseQuantities.Length",
        json!(4100.0),
        OWNER,
        "editor",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", json_body(&bytes));
    assert_eq!(
        json_body(&bytes),
        json!({ "displayValue": "4100 mm", "value": 4100.0, "units": "mm" })
    );

    let (status, bytes) = put_property(
        &state,
        ion_id,
        OUTER_WALL,
        "Pset_WallCommon.FireRating",
        json!("REI90"),
        OWNER,
        "editor",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", json_body(&bytes));
    assert_eq!(
        json_body(&bytes),
        json!({ "displayValue": "REI90", "value": "REI90", "units": null })
    );

    let ifc = String::from_utf8(std::fs::read(&ifc_path).unwrap()).unwrap();
    assert_eq!(ifc.matches("IFCLABEL('REI90')").count(), 1);
    assert!(!ifc.contains("IFCLABEL('REI60')"));
    assert!(ifc.contains("IFCLABEL('EI30')"));
    assert_eq!(ifc.matches("IFCPROPERTYSET(").count(), 2);
    assert!(ifc.contains("4100."));
    assert!(ifc.contains("2500."));
    assert!(!ifc.contains("4000."));

    let wall = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/{}", encoded(OUTER_WALL)),
    )
    .await;
    assert_eq!(
        wall["properties"]["Pset_WallCommon.FireRating"]["value"],
        "REI90"
    );
    assert_eq!(
        wall["properties"]["Qto_WallBaseQuantities.Length"]["value"],
        4100.0
    );
    let inner = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/{}", encoded(INNER_WALL)),
    )
    .await;
    assert_eq!(
        inner["properties"]["Qto_WallBaseQuantities.Length"]["value"],
        2500.0
    );

    let found = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/search?search=REI90"),
    )
    .await;
    assert_eq!(filtered_ids(&found), vec![OUTER_WALL.to_string()]);
    let stale = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/search?search=REI60"),
    )
    .await;
    assert_eq!(stale["total"], 0);
}

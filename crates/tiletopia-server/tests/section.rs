mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{test_state, token};
use serde_json::Value;
use tiletopia_server::db::JobStatus;
use tiletopia_server::{AppState, Asset, AssetStatus, AssetType, router};
use tower::ServiceExt;
use uuid::Uuid;

const OWNER: &str = "section-owner";
const WALL: &str = "0Wall00000000000000000";

// a 4 m by 0.3 m by 2.5 m wall along x, cut through by an opening 1.2 m wide from z 0.5 to 2.0
const WALL_WITH_AN_OPENING_IFC: &str = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('section.ifc','2026-11-05',(''),(''),'','','');
FILE_SCHEMA(('IFC4'));
ENDSEC;
DATA;
#1=IFCPROJECT('0Project00000000000000',$,'Test',$,$,$,$,(#10),#7);
#2=IFCSITE('0Site00000000000000000',$,'Plot',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
#3=IFCBUILDING('0Building0000000000000',$,'Main',$,$,$,$,$,.ELEMENT.,$,$,$);
#4=IFCBUILDINGSTOREY('0Storey000000000000000',$,'Ground',$,$,$,$,$,.ELEMENT.,0.);
#5=IFCRELAGGREGATES('0Rel1',$,$,$,#1,(#2));
#6=IFCRELAGGREGATES('0Rel2',$,$,$,#2,(#3));
#9=IFCRELAGGREGATES('0Rel3',$,$,$,#3,(#4));
#7=IFCUNITASSIGNMENT((#8));
#8=IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);
#10=IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,1.E-5,#11,$);
#11=IFCAXIS2PLACEMENT3D(#12,$,$);
#12=IFCCARTESIANPOINT((0.,0.,0.));
#13=IFCGEOMETRICREPRESENTATIONSUBCONTEXT('Body','Model',*,*,*,*,#10,$,.MODEL_VIEW.,$);
#20=IFCLOCALPLACEMENT($,#21);
#21=IFCAXIS2PLACEMENT3D(#22,#23,#24);
#22=IFCCARTESIANPOINT((0.,0.,0.));
#23=IFCDIRECTION((0.,0.,1.));
#24=IFCDIRECTION((1.,0.,0.));
#30=IFCRECTANGLEPROFILEDEF(.AREA.,'WallProfile',#31,4.0,0.3);
#31=IFCAXIS2PLACEMENT2D(#32,#33);
#32=IFCCARTESIANPOINT((0.,0.));
#33=IFCDIRECTION((1.,0.));
#40=IFCEXTRUDEDAREASOLID(#30,#41,#42,2.5);
#41=IFCAXIS2PLACEMENT3D(#43,$,$);
#42=IFCDIRECTION((0.,0.,1.));
#43=IFCCARTESIANPOINT((0.,0.,0.));
#50=IFCSHAPEREPRESENTATION(#13,'Body','SweptSolid',(#40));
#51=IFCPRODUCTDEFINITIONSHAPE($,$,(#50));
#100=IFCWALL('0Wall00000000000000000',$,'Wall',$,$,#20,#51,'Tag',$);
#101=IFCRELCONTAINEDINSPATIALSTRUCTURE('0Rel4',$,$,$,(#100),#4);
#110=IFCLOCALPLACEMENT(#20,#111);
#111=IFCAXIS2PLACEMENT3D(#112,#113,#114);
#112=IFCCARTESIANPOINT((0.,-0.5,1.25));
#113=IFCDIRECTION((0.,1.,0.));
#114=IFCDIRECTION((1.,0.,0.));
#127=IFCRECTANGLEPROFILEDEF(.AREA.,'Window',#128,1.2,1.5);
#128=IFCAXIS2PLACEMENT2D(#129,#130);
#129=IFCCARTESIANPOINT((0.,0.));
#130=IFCDIRECTION((1.,0.));
#131=IFCEXTRUDEDAREASOLID(#127,#132,#133,1.0);
#132=IFCAXIS2PLACEMENT3D(#134,$,$);
#133=IFCDIRECTION((0.,0.,1.));
#134=IFCCARTESIANPOINT((0.,0.,0.));
#140=IFCSHAPEREPRESENTATION(#13,'Body','SweptSolid',(#131));
#141=IFCPRODUCTDEFINITIONSHAPE($,$,(#140));
#200=IFCOPENINGELEMENT('0Opening00000000000000',$,'Window',$,$,#110,#141,$,.OPENING.);
#300=IFCRELVOIDSELEMENT('0Voids000000000000000',$,$,$,#100,#200);
ENDSEC;
END-ISO-10303-21;
";

const TRIANGLE_OBJ: &str = "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n";

// ifc-lite stores vertices as f32
const ROUNDING_METRES: f64 = 1e-3;

const X: usize = 0;
const Y: usize = 1;
const Z: usize = 2;

struct TiledModel {
    id: Uuid,
    ion_id: i64,
}

async fn upload_and_tile(state: &Arc<AppState>, file_name: &str, contents: &str) -> TiledModel {
    let boundary = "sectiontestboundary";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"longitude\"\r\n\r\n10\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"latitude\"\r\n\r\n20\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\r\n\
         {contents}\r\n--{boundary}--\r\n"
    );
    let request = Request::builder()
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
        .unwrap();
    let (status, uploaded) = send(state, request).await;
    assert_eq!(status, StatusCode::CREATED, "{uploaded}");
    let id = Uuid::parse_str(uploaded["id"].as_str().unwrap()).unwrap();

    let worker = Arc::clone(&state.job_queue).start().await;
    let job = loop {
        let jobs = state.db.list_jobs_for_asset(id).await.unwrap();
        let current = jobs.into_iter().next().expect("a job for the asset");
        if matches!(current.status, JobStatus::Done | JobStatus::Failed) {
            break current;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    worker.abort();
    assert_eq!(job.status, JobStatus::Done, "error: {:?}", job.error);

    let (_, ion_id) = state.db.get_asset_with_ion_id(id).await.unwrap().unwrap();
    TiledModel { id, ion_id }
}

async fn send(state: &Arc<AppState>, request: Request<Body>) -> (StatusCode, Value) {
    let response = router(Arc::clone(state)).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get_as(state: &Arc<AppState>, uri: &str, subject: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(uri)
        .header(
            "authorization",
            format!("Bearer {}", token(subject, "viewer")),
        )
        .body(Body::empty())
        .unwrap();
    send(state, request).await
}

async fn section(state: &Arc<AppState>, ion_id: i64, axis: &str, position: f64) -> Value {
    let uri = format!("/v1/assets/{ion_id}/section?axis={axis}&position={position}");
    let (status, body) = get_as(state, &uri, OWNER).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    assert_eq!(body["axis"], axis);
    assert_eq!(body["position"], position);
    assert_eq!(body["openChainsDropped"], 0, "{body}");
    body
}

type Ring = Vec<[f64; 3]>;

fn rings_of(polygon: &Value) -> Vec<Ring> {
    assert_eq!(polygon["elementId"], WALL);
    serde_json::from_value(polygon["rings"].clone()).unwrap()
}

fn close(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() < ROUNDING_METRES
}

fn assert_lies_on(ring: &[[f64; 3]], axis: usize, position: f64) {
    assert!(ring.iter().all(|point| point[axis] == position), "{ring:?}");
}

fn assert_outlines_rectangle(
    ring: &[[f64; 3]],
    axes: [usize; 2],
    minimum: [f64; 2],
    maximum: [f64; 2],
) {
    assert!(ring.len() >= 4, "{ring:?}");
    assert_ne!(
        ring.first(),
        ring.last(),
        "rings are not closed by repeating a point"
    );
    for (side, axis) in axes.into_iter().enumerate() {
        let lowest = ring
            .iter()
            .map(|point| point[axis])
            .fold(f64::INFINITY, f64::min);
        let highest = ring
            .iter()
            .map(|point| point[axis])
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(
            close(lowest, minimum[side]) && close(highest, maximum[side]),
            "{ring:?} spans {lowest} to {highest} on axis {axis}"
        );
    }
    for point in ring {
        let on_an_edge = axes.into_iter().enumerate().any(|(side, axis)| {
            close(point[axis], minimum[side]) || close(point[axis], maximum[side])
        });
        assert!(
            on_an_edge,
            "{point:?} is off the outline of {minimum:?} to {maximum:?}"
        );
    }
}

#[tokio::test]
async fn a_cut_below_the_opening_is_one_rectangle_of_the_walls_length_and_thickness() {
    let state = test_state().await;
    let model = upload_and_tile(&state, "section.ifc", WALL_WITH_AN_OPENING_IFC).await;

    let body = section(&state, model.ion_id, "z", 0.25).await;

    let polygons = body["polygons"].as_array().unwrap();
    assert_eq!(polygons.len(), 1, "{body}");
    let rings = rings_of(&polygons[0]);
    assert_eq!(rings.len(), 1, "{rings:?}");
    assert_lies_on(&rings[0], Z, 0.25);
    assert_outlines_rectangle(&rings[0], [X, Y], [-2.0, -0.15], [2.0, 0.15]);

    // 3d tiles boxes are a centre then three half axes, in the frame root.transform places
    let tileset: Value = serde_json::from_slice(
        &std::fs::read(
            state
                .data_dir
                .join(model.id.to_string())
                .join("tileset.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let root_box: Vec<f64> =
        serde_json::from_value(tileset["root"]["boundingVolume"]["box"].clone()).unwrap();
    assert_outlines_rectangle(
        &rings[0],
        [X, Y],
        [root_box[0] - root_box[3], root_box[1] - root_box[7]],
        [root_box[0] + root_box[3], root_box[1] + root_box[7]],
    );
}

#[tokio::test]
async fn a_cut_through_the_opening_leaves_it_as_a_gap_or_a_hole() {
    let state = test_state().await;
    let model = upload_and_tile(&state, "section.ifc", WALL_WITH_AN_OPENING_IFC).await;

    let across = section(&state, model.ion_id, "z", 1.25).await;
    let mut pieces: Vec<Ring> = across["polygons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|polygon| {
            let rings = rings_of(polygon);
            assert_eq!(rings.len(), 1, "{rings:?}");
            rings.into_iter().next().unwrap()
        })
        .collect();
    assert_eq!(pieces.len(), 2, "{across}");
    pieces.sort_by(|left, right| left[0][X].total_cmp(&right[0][X]));
    assert_lies_on(&pieces[0], Z, 1.25);
    assert_lies_on(&pieces[1], Z, 1.25);
    assert_outlines_rectangle(&pieces[0], [X, Y], [-2.0, -0.15], [-0.6, 0.15]);
    assert_outlines_rectangle(&pieces[1], [X, Y], [0.6, -0.15], [2.0, 0.15]);

    let along = section(&state, model.ion_id, "y", 0.0).await;
    let polygons = along["polygons"].as_array().unwrap();
    assert_eq!(polygons.len(), 1, "{along}");
    let rings = rings_of(&polygons[0]);
    assert_eq!(rings.len(), 2, "{rings:?}");
    assert_lies_on(&rings[0], Y, 0.0);
    assert_lies_on(&rings[1], Y, 0.0);
    assert_outlines_rectangle(&rings[0], [X, Z], [-2.0, 0.0], [2.0, 2.5]);
    assert_outlines_rectangle(&rings[1], [X, Z], [-0.6, 0.5], [0.6, 2.0]);
}

#[tokio::test]
async fn positions_outside_the_model_answer_no_polygons() {
    let state = test_state().await;
    let model = upload_and_tile(&state, "section.ifc", WALL_WITH_AN_OPENING_IFC).await;

    for (axis, position) in [("z", 10.0), ("z", -1.0), ("x", 5.0), ("y", -0.5)] {
        let body = section(&state, model.ion_id, axis, position).await;
        assert_eq!(
            body["polygons"],
            Value::Array(Vec::new()),
            "{axis} {position}"
        );
    }
}

#[tokio::test]
async fn a_bad_axis_or_position_answers_400() {
    let state = test_state().await;
    let model = upload_and_tile(&state, "section.ifc", WALL_WITH_AN_OPENING_IFC).await;

    for query in [
        "axis=w&position=1",
        "axis=Z&position=1",
        "position=1",
        "axis=z",
        "axis=z&position=abc",
        "axis=z&position=NaN",
        "axis=z&position=inf",
    ] {
        let uri = format!("/v1/assets/{}/section?{query}", model.ion_id);
        let (status, body) = get_as(&state, &uri, OWNER).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["code"], "INVALID_PARAMETER", "{query}");
    }
}

#[tokio::test]
async fn an_asset_that_is_not_a_tiled_ifc_answers_404() {
    let state = test_state().await;
    let mesh = upload_and_tile(&state, "triangle.obj", TRIANGLE_OBJ).await;
    let never_tiled = state
        .db
        .create_asset(&Asset {
            id: Uuid::new_v4(),
            name: "never tiled".to_string(),
            asset_type: AssetType::Model,
            status: AssetStatus::Ready,
            created_at: chrono::Utc::now(),
            tile_count: 0,
            size_bytes: 0,
            description: String::new(),
            tags: Vec::new(),
            owner_id: Some(OWNER.to_string()),
        })
        .await
        .unwrap();

    for ion_id in [mesh.ion_id, never_tiled] {
        let uri = format!("/v1/assets/{ion_id}/section?axis=z&position=0.5");
        let (status, body) = get_as(&state, &uri, OWNER).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["code"], "ASSET_HAS_NO_MESH_SOURCE");
    }
}

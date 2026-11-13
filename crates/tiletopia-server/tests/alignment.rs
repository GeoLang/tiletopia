mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{test_state, token};
use serde_json::Value;
use tiletopia_server::db::JobStatus;
use tiletopia_server::{AppState, router};
use tower::ServiceExt;
use uuid::Uuid;

const OWNER: &str = "alignment-owner";

const ROAD_IN_MILLIMETRES_IFC: &str = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [Alignment-basedView]'),'2;1');
FILE_NAME('road.ifc','2026-11-13',(''),(''),'','','');
FILE_SCHEMA(('IFC4X3_ADD2'));
ENDSEC;
DATA;
#1=IFCPROJECT('0Project00000000000000',$,'Test',$,$,$,$,(#10),#7);
#2=IFCSITE('0Site00000000000000000',$,'Plot',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
#5=IFCRELAGGREGATES('0Rel1',$,$,$,#1,(#2));
#7=IFCUNITASSIGNMENT((#8));
#8=IFCSIUNIT(*,.LENGTHUNIT.,.MILLI.,.METRE.);
#10=IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,1.E-5,#11,$);
#11=IFCAXIS2PLACEMENT3D(#12,$,$);
#12=IFCCARTESIANPOINT((0.,0.,0.));
#13=IFCGEOMETRICREPRESENTATIONSUBCONTEXT('Body','Model',*,*,*,*,#10,$,.MODEL_VIEW.,$);
#20=IFCLOCALPLACEMENT($,#21);
#21=IFCAXIS2PLACEMENT3D(#22,$,$);
#22=IFCCARTESIANPOINT((0.,0.,0.));
#30=IFCRECTANGLEPROFILEDEF(.AREA.,'WallProfile',#31,4000.,300.);
#31=IFCAXIS2PLACEMENT2D(#32,$);
#32=IFCCARTESIANPOINT((0.,0.));
#40=IFCEXTRUDEDAREASOLID(#30,#41,#42,2500.);
#41=IFCAXIS2PLACEMENT3D(#43,$,$);
#42=IFCDIRECTION((0.,0.,1.));
#43=IFCCARTESIANPOINT((0.,0.,0.));
#50=IFCSHAPEREPRESENTATION(#13,'Body','SweptSolid',(#40));
#51=IFCPRODUCTDEFINITIONSHAPE($,$,(#50));
#100=IFCWALL('0Wall00000000000000000',$,'Wall',$,$,#20,#51,$,$);
#101=IFCRELCONTAINEDINSPATIALSTRUCTURE('0Rel2',$,$,$,(#100),#2);
#200=IFCALIGNMENT('0Alignment000000000000',$,'Road',$,$,$,$,$);
#201=IFCALIGNMENTHORIZONTAL('0Horizontal00000000000',$,$,$,$,$,$);
#202=IFCRELNESTS('0Rel3',$,$,$,#200,(#201));
#210=IFCALIGNMENTSEGMENT('0Segment1000000000000',$,$,$,$,$,$,#211);
#211=IFCALIGNMENTHORIZONTALSEGMENT($,$,#212,1.5707963267948966,0.,0.,10000.,$,.LINE.);
#212=IFCCARTESIANPOINT((0.,0.));
#220=IFCALIGNMENTSEGMENT('0Segment2000000000000',$,$,$,$,$,$,#221);
#221=IFCALIGNMENTHORIZONTALSEGMENT($,$,#222,1.5707963267948966,0.,0.,10000.,$,.LINE.);
#222=IFCCARTESIANPOINT((0.,10000.));
#230=IFCRELNESTS('0Rel4',$,$,$,#201,(#210,#220));
#240=IFCRELAGGREGATES('0Rel5',$,$,$,#1,(#200));
ENDSEC;
END-ISO-10303-21;
";

const TRIANGLE_OBJ: &str = "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n";

async fn upload_and_tile(state: &Arc<AppState>, file_name: &str, contents: &str) -> i64 {
    let boundary = "alignmenttestboundary";
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
    ion_id
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

async fn alignments_as(state: &Arc<AppState>, ion_id: i64, subject: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(format!("/v1/assets/{ion_id}/alignments"))
        .header(
            "authorization",
            format!("Bearer {}", token(subject, "viewer")),
        )
        .body(Body::empty())
        .unwrap();
    send(state, request).await
}

fn close(actual: &Value, expected: f64) -> bool {
    (actual.as_f64().unwrap() - expected).abs() < 1.0e-6
}

#[tokio::test]
async fn a_road_in_millimetres_answers_its_stations_in_metres() {
    let state = test_state().await;
    let ion_id = upload_and_tile(&state, "road.ifc", ROAD_IN_MILLIMETRES_IFC).await;

    let (status, body) = alignments_as(&state, ion_id, OWNER).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let alignments = body.as_array().unwrap();
    assert_eq!(alignments.len(), 1, "{body}");
    let road = &alignments[0];
    assert_eq!(road["id"], "0Alignment000000000000");
    assert_eq!(road["name"], "Road");
    assert_eq!(road["approximate"], false);
    assert!(close(&road["length"], 20.0), "{road}");
    let stations = road["stations"].as_array().unwrap();
    assert_eq!(stations.len(), 3, "{road}");
    for (station, metres) in stations.iter().zip([0.0, 10.0, 20.0]) {
        assert!(close(&station["distance"], metres), "{station}");
        assert!(close(&station["position"][0], 0.0), "{station}");
        assert!(close(&station["position"][1], metres), "{station}");
        assert!(close(&station["tangent"][0], 0.0), "{station}");
        assert!(close(&station["tangent"][1], 1.0), "{station}");
    }
}

#[tokio::test]
async fn another_user_and_a_mesh_get_no_alignments() {
    let state = test_state().await;
    let road = upload_and_tile(&state, "road.ifc", ROAD_IN_MILLIMETRES_IFC).await;
    let mesh = upload_and_tile(&state, "triangle.obj", TRIANGLE_OBJ).await;

    let (status, body) = alignments_as(&state, road, "someone-else").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, body) = alignments_as(&state, mesh, OWNER).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "INVALID_PARAMETER");
}

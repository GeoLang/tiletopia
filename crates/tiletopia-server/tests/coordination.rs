mod common;

use std::io::Read;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{test_state, texture_png, token, zip_of};
use serde_json::{Value, json};
use tiletopia_server::db::JobStatus;
use tiletopia_server::element_store::store_path;
use tiletopia_server::{AppState, Asset, AssetStatus, AssetType, model_versions, router};
use tower::ServiceExt;
use uuid::Uuid;

const OWNER: &str = "coordination-owner";
const STRANGER: &str = "coordination-stranger";

const PROJECT: &str = "0project00000000000000";
const SITE: &str = "0site00000000000000000";
const BUILDING: &str = "0building0000000000000";
const STOREY: &str = "0storey000000000000000";
const KEPT_WALL: &str = "0keptwall0000000000000";
const CHANGED_WALL: &str = "0changedwall0000000000";
const REMOVED_WALL: &str = "0removedwall0000000000";
const ADDED_WALL: &str = "0addedwall000000000000";
const UNKNOWN_ELEMENT: &str = "3unknown00000000000000";

const TOPIC_GUID: &str = "5a0e0c5e-1b1b-4a2a-8a55-0f3a7c9d1e01";
const VIEWPOINT_GUID: &str = "6b1f1d6f-2c2c-4b3b-9b66-1a4b8dae2f02";
const COMMENT_GUID: &str = "7c2a2e7a-3d3d-4c4c-ac77-2b5c9ebf3a03";
const RELATED_GUID: &str = "8d3b3f8b-4e4e-4d5d-bd88-3c6dafc04b04";

const OFFICIAL_BCF: &[u8] = include_bytes!("fixtures/bcf/component-selection.bcf");
const OFFICIAL_TOPIC_GUID: &str = "647bca1c-cac3-4f16-84a8-912e081edd57";
const OFFICIAL_VIEWPOINT_GUID: &str = "7b2c1bf5-5854-433d-8136-981c957ed910";

struct ModelRevision<'a> {
    fire_rating: &'a str,
    changed_wall_depth: &'a str,
    last_wall_id: &'a str,
    last_wall_name: &'a str,
}

const FIRST_REVISION: ModelRevision<'static> = ModelRevision {
    fire_rating: "REI30",
    changed_wall_depth: "3.0",
    last_wall_id: REMOVED_WALL,
    last_wall_name: "Removed Wall",
};

const SECOND_REVISION: ModelRevision<'static> = ModelRevision {
    fire_rating: "REI90",
    changed_wall_depth: "5.0",
    last_wall_id: ADDED_WALL,
    last_wall_name: "Added Wall",
};

fn model_ifc(revision: &ModelRevision<'_>) -> String {
    let ModelRevision {
        fire_rating,
        changed_wall_depth,
        last_wall_id,
        last_wall_name,
    } = revision;
    format!(
        "ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');
FILE_NAME('model.ifc','2026-11-05',(''),(''),'','','');
FILE_SCHEMA(('IFC2X3'));
ENDSEC;
DATA;
#1=IFCPROJECT('{PROJECT}',$,'Model',$,$,$,$,$,$);
#2=IFCSITE('{SITE}',$,'Plot',$,$,$,$,$,.ELEMENT.,$,$,$,$,$);
#3=IFCBUILDING('{BUILDING}',$,'Main',$,$,$,$,$,.ELEMENT.,$,$,$);
#4=IFCBUILDINGSTOREY('{STOREY}',$,'Ground',$,$,$,$,$,.ELEMENT.,0.);
#5=IFCRELAGGREGATES('0rel1',$,$,$,#1,(#2));
#6=IFCRELAGGREGATES('0rel2',$,$,$,#2,(#3));
#7=IFCRELAGGREGATES('0rel3',$,$,$,#3,(#4));
#10=IFCCARTESIANPOINT((0.,0.,0.));
#11=IFCAXIS2PLACEMENT3D(#10,$,$);
#12=IFCLOCALPLACEMENT($,#11);
#20=IFCRECTANGLEPROFILEDEF(.AREA.,$,#11,2.0,1.0);
#21=IFCDIRECTION((0.,0.,1.));
#22=IFCEXTRUDEDAREASOLID(#20,#11,#21,3.0);
#23=IFCEXTRUDEDAREASOLID(#20,#11,#21,{changed_wall_depth});
#30=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#22));
#31=IFCPRODUCTDEFINITIONSHAPE($,$,(#30));
#32=IFCSHAPEREPRESENTATION($,'Body','SweptSolid',(#23));
#33=IFCPRODUCTDEFINITIONSHAPE($,$,(#32));
#40=IFCWALL('{KEPT_WALL}',$,'Kept Wall',$,$,#12,#31,$);
#41=IFCWALL('{CHANGED_WALL}',$,'Changed Wall',$,$,#12,#33,$);
#42=IFCWALL('{last_wall_id}',$,'{last_wall_name}',$,$,#12,#31,$);
#43=IFCRELCONTAINEDINSPATIALSTRUCTURE('0rel4',$,$,$,(#40,#41,#42),#4);
#50=IFCPROPERTYSINGLEVALUE('FireRating',$,IFCLABEL('{fire_rating}'),$);
#51=IFCPROPERTYSET('0pset1',$,'Pset_WallCommon',$,(#50));
#52=IFCRELDEFINESBYPROPERTIES('0rel5',$,$,$,(#41),#51);
ENDSEC;
END-ISO-10303-21;
"
    )
}

const BOUNDARY: &str = "coordinationboundary";

fn multipart_body(text_fields: &[(&str, &str)], file_name: &str, file: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in text_fields {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

struct Answer {
    status: StatusCode,
    bytes: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
}

async fn send(state: &Arc<AppState>, request: Request<Body>) -> Answer {
    let response = router(Arc::clone(state)).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    Answer {
        status,
        bytes: bytes.to_vec(),
    }
}

fn bearer(subject: &str, role: &str) -> String {
    format!("Bearer {}", token(subject, role))
}

async fn get_as(state: &Arc<AppState>, uri: &str, subject: &str) -> Answer {
    let request = Request::builder()
        .uri(uri)
        .header("authorization", bearer(subject, "viewer"))
        .body(Body::empty())
        .unwrap();
    send(state, request).await
}

async fn owner_get(state: &Arc<AppState>, uri: &str) -> Value {
    let answer = get_as(state, uri, OWNER).await;
    assert_eq!(answer.status, StatusCode::OK, "{uri}: {}", answer.json());
    answer.json()
}

async fn send_json(
    state: &Arc<AppState>,
    method: &str,
    uri: &str,
    subject: &str,
    body: &Value,
) -> Answer {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", bearer(subject, "editor"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(state, request).await
}

async fn upload(
    state: &Arc<AppState>,
    subject: &str,
    revision: &ModelRevision<'_>,
    base_asset_id: Option<&str>,
) -> Answer {
    let mut fields = vec![("longitude", "10"), ("latitude", "20")];
    if let Some(base_asset_id) = base_asset_id {
        fields.push(("base_asset_id", base_asset_id));
    }
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/assets")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("authorization", bearer(subject, "editor"))
        .body(Body::from(multipart_body(
            &fields,
            "model.ifc",
            model_ifc(revision).as_bytes(),
        )))
        .unwrap();
    send(state, request).await
}

struct UploadedModel {
    id: Uuid,
    ion_id: i64,
    input_path: std::path::PathBuf,
}

async fn upload_and_tile(
    state: &Arc<AppState>,
    revision: &ModelRevision<'_>,
    base_asset_id: Option<&str>,
) -> UploadedModel {
    let answer = upload(state, OWNER, revision, base_asset_id).await;
    assert_eq!(answer.status, StatusCode::CREATED);
    let id = Uuid::parse_str(answer.json()["id"].as_str().unwrap()).unwrap();

    let worker = Arc::clone(&state.job_queue).start().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let job = loop {
        let jobs = state.db.list_jobs_for_asset(id).await.unwrap();
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
    assert_eq!(job.status, JobStatus::Done, "error: {:?}", job.error);

    let (_, ion_id) = state.db.get_asset_with_ion_id(id).await.unwrap().unwrap();
    UploadedModel {
        id,
        ion_id,
        input_path: state.data_dir.join(id.to_string()).join(job.input_path),
    }
}

async fn import_bcf(state: &Arc<AppState>, ion_id: i64, subject: &str, bytes: &[u8]) -> Answer {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/assets/{ion_id}/bcf/import"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header("authorization", bearer(subject, "editor"))
        .body(Body::from(multipart_body(&[], "issues.bcf", bytes)))
        .unwrap();
    send(state, request).await
}

fn version_file() -> Vec<u8> {
    b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><Version VersionId=\"3.0\"/>".to_vec()
}

fn markup_xml() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Markup>
  <Header><Files>
    <File IfcProject="{PROJECT}" IfcSpatialStructureElement="{STOREY}" IsExternal="false">
      <Filename>model.ifc</Filename><Date>2026-11-05T10:00:00Z</Date><Reference>..\model.ifc</Reference>
    </File>
  </Files></Header>
  <Topic Guid="{TOPIC_GUID}" ServerAssignedId="ISSUE-7" TopicType="CLASH" TopicStatus="OPEN">
    <ReferenceLinks><ReferenceLink>https://example.test/issue/7</ReferenceLink></ReferenceLinks>
    <Title>Duct &amp; wall clash</Title>
    <Priority>HIGH</Priority>
    <Index>3</Index>
    <Labels><Label>MEP</Label><Label>Structure</Label></Labels>
    <CreationDate>2026-11-01T09:30:00Z</CreationDate>
    <CreationAuthor>ada@example.test</CreationAuthor>
    <ModifiedDate>2026-11-02T09:30:00Z</ModifiedDate>
    <ModifiedAuthor>grace@example.test</ModifiedAuthor>
    <DueDate>2026-12-01T00:00:00Z</DueDate>
    <AssignedTo>grace@example.test</AssignedTo>
    <Stage>Design</Stage>
    <Description>The duct runs through the wall</Description>
    <BimSnippet SnippetType="IssueSnippet" IsExternal="true">
      <Reference>https://example.test/snippet</Reference>
      <ReferenceSchema>https://example.test/schema</ReferenceSchema>
    </BimSnippet>
    <DocumentReferences>
      <DocumentReference Guid="{RELATED_GUID}"><Url>https://example.test/doc</Url><Description>Spec</Description></DocumentReference>
      <DocumentReference Guid="{COMMENT_GUID}"><DocumentGuid>{VIEWPOINT_GUID}</DocumentGuid></DocumentReference>
    </DocumentReferences>
    <RelatedTopics><RelatedTopic Guid="{RELATED_GUID}"/></RelatedTopics>
    <Comments>
      <Comment Guid="{COMMENT_GUID}">
        <Date>2026-11-03T08:00:00Z</Date>
        <Author>ada@example.test</Author>
        <Comment>Please move the duct</Comment>
        <Viewpoint Guid="{VIEWPOINT_GUID}"/>
        <ModifiedDate>2026-11-03T09:00:00Z</ModifiedDate>
        <ModifiedAuthor>ada@example.test</ModifiedAuthor>
      </Comment>
    </Comments>
    <Viewpoints>
      <ViewPoint Guid="{VIEWPOINT_GUID}">
        <Viewpoint>view.bcfv</Viewpoint>
        <Snapshot>snap.png</Snapshot>
        <Index>1</Index>
      </ViewPoint>
    </Viewpoints>
  </Topic>
</Markup>"#
    )
}

fn visualization_xml() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<VisualizationInfo Guid="{VIEWPOINT_GUID}">
  <Components>
    <Selection>
      <Component IfcGuid="{KEPT_WALL}"/>
      <Component IfcGuid="{UNKNOWN_ELEMENT}"/>
      <Component><AuthoringToolId>revit-42</AuthoringToolId></Component>
    </Selection>
    <Visibility DefaultVisibility="true">
      <ViewSetupHints SpacesVisible="true" SpaceBoundariesVisible="false" OpeningsVisible="true"/>
      <Exceptions><Component IfcGuid="{CHANGED_WALL}"/></Exceptions>
    </Visibility>
    <Coloring>
      <Color Color="FF00FF80">
        <Components>
          <Component IfcGuid="{CHANGED_WALL}">
            <OriginatingSystem>Revit</OriginatingSystem>
            <AuthoringToolId>revit-43</AuthoringToolId>
          </Component>
        </Components>
      </Color>
    </Coloring>
  </Components>
  <PerspectiveCamera>
    <CameraViewPoint><X>1.5</X><Y>-2.25</Y><Z>3.75</Z></CameraViewPoint>
    <CameraDirection><X>0</X><Y>1</Y><Z>0</Z></CameraDirection>
    <CameraUpVector><X>0</X><Y>0</Y><Z>1</Z></CameraUpVector>
    <FieldOfView>60.5</FieldOfView>
    <AspectRatio>1.5</AspectRatio>
  </PerspectiveCamera>
  <Lines>
    <Line>
      <StartPoint><X>0</X><Y>0</Y><Z>0</Z></StartPoint>
      <EndPoint><X>1</X><Y>2</Y><Z>3</Z></EndPoint>
    </Line>
  </Lines>
  <ClippingPlanes>
    <ClippingPlane>
      <Location><X>0.5</X><Y>0.5</Y><Z>1.5</Z></Location>
      <Direction><X>0</X><Y>0</Y><Z>-1</Z></Direction>
    </ClippingPlane>
  </ClippingPlanes>
  <Bitmaps>
    <Bitmap>
      <Format>png</Format>
      <Reference>floor.png</Reference>
      <Location><X>0</X><Y>0</Y><Z>0</Z></Location>
      <Normal><X>0</X><Y>0</Y><Z>1</Z></Normal>
      <Up><X>0</X><Y>1</Y><Z>0</Z></Up>
      <Height>1</Height>
    </Bitmap>
  </Bitmaps>
</VisualizationInfo>"#
    )
}

const EXTENSIONS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Extensions>
  <TopicTypes><TopicType>CLASH</TopicType></TopicTypes>
  <TopicStatuses><TopicStatus>OPEN</TopicStatus><TopicStatus>CLOSED</TopicStatus></TopicStatuses>
  <Priorities><Priority>HIGH</Priority><Priority>CRITICAL</Priority></Priorities>
  <TopicLabels><TopicLabel>MEP</TopicLabel></TopicLabels>
  <Users><User>grace@example.test</User></Users>
  <SnippetTypes><SnippetType>IssueSnippet</SnippetType></SnippetTypes>
  <Stages><Stage>Design</Stage></Stages>
</Extensions>"#;

const PROJECT_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ProjectInfo><Project ProjectId="project-9"><Name>Tower</Name></Project></ProjectInfo>"#;

const DOCUMENTS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DocumentInfo><Documents><Document Guid="11111111-1111-4111-8111-111111111111"><Filename>a.pdf</Filename></Document></Documents></DocumentInfo>"#;

fn full_bcf() -> Vec<u8> {
    let markup = markup_xml();
    let visualization = visualization_xml();
    let snapshot = texture_png();
    let markup_path = format!("{TOPIC_GUID}/markup.bcf");
    let viewpoint_path = format!("{TOPIC_GUID}/view.bcfv");
    let snapshot_path = format!("{TOPIC_GUID}/snap.png");
    let version = version_file();
    zip_of(&[
        ("bcf.version", version.as_slice()),
        ("extensions.xml", EXTENSIONS_XML.as_bytes()),
        ("project.bcfp", PROJECT_XML.as_bytes()),
        ("documents.xml", DOCUMENTS_XML.as_bytes()),
        (markup_path.as_str(), markup.as_bytes()),
        (viewpoint_path.as_str(), visualization.as_bytes()),
        (snapshot_path.as_str(), snapshot.as_slice()),
    ])
}

fn zip_entry(bytes: &[u8], name: &str) -> Vec<u8> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).unwrap();
    let mut entry = archive.by_name(name).unwrap();
    let mut content = Vec::new();
    entry.read_to_end(&mut content).unwrap();
    content
}

#[tokio::test]
async fn a_bcf_file_round_trips_every_stored_field_through_import_and_export() {
    let state = test_state().await;
    let first_model = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let second_model = upload_and_tile(&state, &FIRST_REVISION, None).await;

    let answer = import_bcf(&state, first_model.ion_id, OWNER, &full_bcf()).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    let report = answer.json();
    assert_eq!(report["topics"], 1);
    assert_eq!(report["created"], 1);
    assert_eq!(report["comments"], 1);
    assert_eq!(report["viewpoints"], 1);
    assert_eq!(report["snapshots"], 1);
    assert_eq!(
        report["elementReferences"],
        json!({ "total": 6, "distinctIds": 5, "unknownIds": 1, "withoutIfcGuid": 1 })
    );
    assert_eq!(report["dropped"]["documents"], 1);
    assert_eq!(report["dropped"]["internalDocumentReferences"], 1);
    assert_eq!(report["dropped"]["bitmaps"], 1);

    let topic_uri = format!("/v1/assets/{}/bcf/topics/{TOPIC_GUID}", first_model.ion_id);
    let topic = owner_get(&state, &topic_uri).await;
    assert_eq!(topic["title"], "Duct & wall clash");
    assert_eq!(topic["serverAssignedId"], "ISSUE-7");
    assert_eq!(topic["topicType"], "CLASH");
    assert_eq!(topic["labels"], json!(["MEP", "Structure"]));
    assert_eq!(topic["files"][0]["ifcProject"], PROJECT);
    assert_eq!(topic["files"][0]["isExternal"], false);
    assert_eq!(topic["externalBimSnippet"]["snippetType"], "IssueSnippet");
    assert_eq!(
        topic["externalDocuments"][0]["url"],
        "https://example.test/doc"
    );
    assert_eq!(topic["relatedTopics"], json!([RELATED_GUID]));
    assert_eq!(topic["comments"][0]["comment"], "Please move the duct");
    assert_eq!(topic["comments"][0]["viewpointGuid"], VIEWPOINT_GUID);

    let viewpoint = &topic["viewpoints"][0];
    assert_eq!(viewpoint["snapshotFormat"], "png");
    assert_eq!(viewpoint["camera"]["kind"], "perspective");
    assert_eq!(
        viewpoint["camera"]["cameraViewPoint"],
        json!([1.5, -2.25, 3.75])
    );
    assert_eq!(viewpoint["camera"]["fieldOfView"], 60.5);
    assert_eq!(viewpoint["selection"][0]["ifcGuid"], KEPT_WALL);
    assert_eq!(viewpoint["selection"][2]["authoringToolId"], "revit-42");
    assert_eq!(viewpoint["visibility"]["defaultVisibility"], true);
    assert_eq!(viewpoint["visibility"]["openingsVisible"], true);
    assert_eq!(
        viewpoint["visibility"]["exceptions"][0]["ifcGuid"],
        CHANGED_WALL
    );
    assert_eq!(viewpoint["coloring"][0]["color"], "FF00FF80");
    assert_eq!(
        viewpoint["coloring"][0]["components"][0]["originatingSystem"],
        "Revit"
    );
    assert_eq!(viewpoint["lines"][0]["endPoint"], json!([1.0, 2.0, 3.0]));
    assert_eq!(
        viewpoint["clippingPlanes"][0]["direction"],
        json!([0.0, 0.0, -1.0])
    );

    let exported = get_as(
        &state,
        &format!("/v1/assets/{}/bcf/export", first_model.ion_id),
        OWNER,
    )
    .await;
    assert_eq!(exported.status, StatusCode::OK);
    let extensions = String::from_utf8(zip_entry(&exported.bytes, "extensions.xml")).unwrap();
    assert!(
        extensions.contains("<Priority>CRITICAL</Priority>"),
        "{extensions}"
    );
    let version = String::from_utf8(zip_entry(&exported.bytes, "bcf.version")).unwrap();
    assert!(version.contains("VersionId=\"3.0\""), "{version}");
    let project = String::from_utf8(zip_entry(&exported.bytes, "project.bcfp")).unwrap();
    assert!(project.contains("ProjectId=\"project-9\""), "{project}");

    let answer = import_bcf(&state, second_model.ion_id, OWNER, &exported.bytes).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    let report = answer.json();
    assert_eq!(report["elementReferences"]["unknownIds"], 1);
    assert_eq!(report["dropped"]["documents"], 0);

    let reimported = owner_get(
        &state,
        &format!("/v1/assets/{}/bcf/topics/{TOPIC_GUID}", second_model.ion_id),
    )
    .await;
    assert_eq!(reimported, topic);

    let snapshot = get_as(
        &state,
        &format!(
            "/v1/assets/{}/bcf/topics/{TOPIC_GUID}/viewpoints/{VIEWPOINT_GUID}/snapshot",
            second_model.ion_id
        ),
        OWNER,
    )
    .await;
    assert_eq!(snapshot.status, StatusCode::OK);
    assert_eq!(snapshot.bytes, texture_png());
}

#[tokio::test]
async fn an_official_bcf_test_case_keeps_its_viewpoint_state() {
    let state = test_state().await;
    let model = upload_and_tile(&state, &FIRST_REVISION, None).await;

    let answer = import_bcf(&state, model.ion_id, OWNER, OFFICIAL_BCF).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.json());
    assert_eq!(
        answer.json()["elementReferences"],
        json!({ "total": 7, "distinctIds": 7, "unknownIds": 7, "withoutIfcGuid": 0 })
    );

    let topic = owner_get(
        &state,
        &format!(
            "/v1/assets/{}/bcf/topics/{OFFICIAL_TOPIC_GUID}",
            model.ion_id
        ),
    )
    .await;
    assert_eq!(topic["title"], "Component Selection");
    assert_eq!(topic["serverAssignedId"], "2");
    assert_eq!(topic["topicType"], "WARNING");
    assert_eq!(topic["files"][1]["filename"], "MEP.ifc");

    let viewpoint = &topic["viewpoints"][0];
    assert_eq!(viewpoint["guid"], OFFICIAL_VIEWPOINT_GUID);
    assert_eq!(viewpoint["camera"]["kind"], "orthogonal");
    let scale = viewpoint["camera"]["viewToWorldScale"].as_f64().unwrap();
    assert!((scale - 18.97427487373352).abs() < 1e-9, "{scale}");
    assert_eq!(viewpoint["selection"].as_array().unwrap().len(), 3);
    assert_eq!(
        viewpoint["selection"][0]["ifcGuid"],
        "0KkZ20so9BsO1d1hFcfLOl"
    );
    assert_eq!(viewpoint["visibility"]["defaultVisibility"], true);
    assert_eq!(viewpoint["visibility"]["spacesVisible"], true);
    assert_eq!(
        viewpoint["visibility"]["exceptions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let snapshot = get_as(
        &state,
        &format!(
            "/v1/assets/{}/bcf/topics/{OFFICIAL_TOPIC_GUID}/viewpoints/{OFFICIAL_VIEWPOINT_GUID}/snapshot",
            model.ion_id
        ),
        OWNER,
    )
    .await;
    assert_eq!(snapshot.status, StatusCode::OK);
    assert_eq!(
        snapshot.bytes,
        zip_entry(
            OFFICIAL_BCF,
            &format!("{OFFICIAL_TOPIC_GUID}/snapshot-{OFFICIAL_VIEWPOINT_GUID}.png")
        )
    );

    let exported = get_as(
        &state,
        &format!("/v1/assets/{}/bcf/export", model.ion_id),
        OWNER,
    )
    .await;
    assert_eq!(exported.status, StatusCode::OK);
    let viewpoint_file = String::from_utf8(zip_entry(
        &exported.bytes,
        &format!("{OFFICIAL_TOPIC_GUID}/Viewpoint_{OFFICIAL_VIEWPOINT_GUID}.bcfv"),
    ))
    .unwrap();
    assert!(
        viewpoint_file.contains("<OrthogonalCamera>"),
        "{viewpoint_file}"
    );
    assert!(
        viewpoint_file.contains("IfcGuid=\"1bbI761TbBCOoIa5Kt6PXt\""),
        "{viewpoint_file}"
    );
}

fn scratch_is_empty(state: &AppState) -> bool {
    match std::fs::read_dir(state.data_dir.join("coordination_scratch")) {
        Ok(mut entries) => entries.next().is_none(),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

#[tokio::test]
async fn a_hostile_bcf_file_stores_nothing_and_leaves_no_scratch_files() {
    let state = test_state().await;
    let model = upload_and_tile(&state, &FIRST_REVISION, None).await;

    let version = version_file();
    let markup_path = format!("{TOPIC_GUID}/markup.bcf");
    let doctype_markup = "<?xml version=\"1.0\"?><!DOCTYPE Markup [<!ENTITY a \"a\">]><Markup/>";
    let escaping_markup = markup_xml().replace("view.bcfv", "../../secret.bcfv");

    let climbing_zip = zip_of(&[
        ("bcf.version", version.as_slice()),
        ("../evil.bcf", "evil".as_bytes()),
    ]);
    let doctype_zip = zip_of(&[
        ("bcf.version", version.as_slice()),
        (markup_path.as_str(), doctype_markup.as_bytes()),
    ]);
    let escaping_zip = zip_of(&[
        ("bcf.version", version.as_slice()),
        (markup_path.as_str(), escaping_markup.as_bytes()),
    ]);
    let old_version_zip = zip_of(&[(
        "bcf.version",
        "<?xml version=\"1.0\"?><Version VersionId=\"2.1\"/>".as_bytes(),
    )]);
    let current_markup = markup_xml();
    let no_version_zip = zip_of(&[(markup_path.as_str(), current_markup.as_bytes())]);

    let cases = [
        ("not a zip", "this is not a zip".as_bytes()),
        ("climbing zip entry", climbing_zip.as_slice()),
        ("document type declaration", doctype_zip.as_slice()),
        (
            "viewpoint outside the topic folder",
            escaping_zip.as_slice(),
        ),
        ("BCF 2.1", old_version_zip.as_slice()),
        ("no bcf.version", no_version_zip.as_slice()),
    ];
    for (name, bytes) in cases {
        let answer = import_bcf(&state, model.ion_id, OWNER, bytes).await;
        assert_eq!(
            answer.status,
            StatusCode::BAD_REQUEST,
            "{name}: {}",
            answer.json()
        );
        assert_eq!(answer.json()["code"], "INVALID_BCF", "{name}");
        assert!(scratch_is_empty(&state), "{name}");
    }
    let topics = owner_get(&state, &format!("/v1/assets/{}/bcf/topics", model.ion_id)).await;
    assert_eq!(topics["total"], 0);
    assert!(
        !state
            .data_dir
            .join("coordination_scratch/evil.bcf")
            .exists()
    );
}

#[tokio::test]
async fn an_import_whose_snapshots_cannot_be_stored_stores_no_topic() {
    let state = test_state().await;
    let model = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let snapshots = state.data_dir.join("bcf_snapshots");
    std::fs::create_dir_all(&snapshots).unwrap();
    // a file where the model's snapshot folder belongs fails every snapshot write
    std::fs::write(snapshots.join(model.id.to_string()), b"in the way").unwrap();

    let answer = import_bcf(&state, model.ion_id, OWNER, &full_bcf()).await;

    assert_eq!(answer.status, StatusCode::INTERNAL_SERVER_ERROR);
    let topics = owner_get(&state, &format!("/v1/assets/{}/bcf/topics", model.ion_id)).await;
    assert_eq!(topics["total"], 0);
    assert!(scratch_is_empty(&state));
}

fn minimal_topic(guid: &str, title: &str) -> Value {
    json!({
        "guid": guid,
        "topicType": "ISSUE",
        "topicStatus": "OPEN",
        "title": title,
        "creationDate": "2026-11-05T12:00:00Z",
        "creationAuthor": OWNER,
        "comments": [{
            "guid": COMMENT_GUID,
            "date": "2026-11-05T12:01:00Z",
            "author": OWNER,
            "comment": "first",
        }],
        "viewpoints": [{
            "guid": VIEWPOINT_GUID,
            "camera": {
                "kind": "orthogonal",
                "cameraViewPoint": [1.0, 2.0, 3.0],
                "cameraDirection": [0.0, 1.0, 0.0],
                "cameraUpVector": [0.0, 0.0, 1.0],
                "viewToWorldScale": 10.0,
                "aspectRatio": 1.5,
            },
            "selection": [{ "ifcGuid": KEPT_WALL }],
        }],
    })
}

#[tokio::test]
async fn a_topic_written_through_the_route_reaches_the_export_and_can_be_deleted() {
    let state = test_state().await;
    let model = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let topic_uri = format!("/v1/assets/{}/bcf/topics/{TOPIC_GUID}", model.ion_id);

    let created = send_json(
        &state,
        "PUT",
        &topic_uri,
        OWNER,
        &minimal_topic(TOPIC_GUID, "Written by the viewer"),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());

    let rewritten = send_json(
        &state,
        "PUT",
        &topic_uri,
        OWNER,
        &minimal_topic(TOPIC_GUID, "Renamed"),
    )
    .await;
    assert_eq!(rewritten.status, StatusCode::OK);

    let listed = owner_get(&state, &format!("/v1/assets/{}/bcf/topics", model.ion_id)).await;
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["items"][0]["title"], "Renamed");
    assert_eq!(listed["items"][0]["commentCount"], 1);

    let wrong_guid = send_json(
        &state,
        "PUT",
        &topic_uri,
        OWNER,
        &minimal_topic(RELATED_GUID, "Other"),
    )
    .await;
    assert_eq!(wrong_guid.status, StatusCode::BAD_REQUEST);

    let blank_title = send_json(
        &state,
        "PUT",
        &topic_uri,
        OWNER,
        &minimal_topic(TOPIC_GUID, " "),
    )
    .await;
    assert_eq!(blank_title.status, StatusCode::BAD_REQUEST);

    let malformed_path_guid = send_json(
        &state,
        "PUT",
        &format!("/v1/assets/{}/bcf/topics/not-a-guid", model.ion_id),
        OWNER,
        &minimal_topic(TOPIC_GUID, "Other"),
    )
    .await;
    assert_eq!(malformed_path_guid.status, StatusCode::BAD_REQUEST);
    assert_eq!(malformed_path_guid.json()["code"], "INVALID_BCF");

    let exported = get_as(
        &state,
        &format!("/v1/assets/{}/bcf/export", model.ion_id),
        OWNER,
    )
    .await;
    let markup = String::from_utf8(zip_entry(
        &exported.bytes,
        &format!("{TOPIC_GUID}/markup.bcf"),
    ))
    .unwrap();
    assert!(markup.contains("<Title>Renamed</Title>"), "{markup}");
    let extensions = String::from_utf8(zip_entry(&exported.bytes, "extensions.xml")).unwrap();
    assert!(
        extensions.contains("<TopicType>ISSUE</TopicType>"),
        "{extensions}"
    );

    let deleted = send(
        &state,
        Request::builder()
            .method("DELETE")
            .uri(&topic_uri)
            .header("authorization", bearer(OWNER, "editor"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(
        get_as(&state, &topic_uri, OWNER).await.status,
        StatusCode::NOT_FOUND
    );
}

async fn wait_for_report(state: &Arc<AppState>, report_id: &str) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let report = owner_get(state, &format!("/v1/change-reports/{report_id}")).await;
        let status = report["status"].as_str().unwrap().to_string();
        if status == "COMPLETE" || status == "ERROR" {
            return report;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the comparison never settled"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_re_upload_is_a_new_version_and_a_comparison_reports_what_changed() {
    let state = test_state().await;
    let first = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let first_ion_id = first.ion_id.to_string();
    let second = upload_and_tile(&state, &SECOND_REVISION, Some(&first_ion_id)).await;

    let revisions = owner_get(
        &state,
        &format!("/v1/assets/{}/tileset-revisions", first.ion_id),
    )
    .await;
    assert_eq!(revisions["total"], 2);
    assert_eq!(revisions["items"][0]["assetId"], second.ion_id);
    assert_eq!(revisions["items"][0]["versionNumber"], 2);
    assert_eq!(revisions["items"][1]["assetId"], first.ion_id);
    assert_eq!(revisions["items"][1]["versionNumber"], 1);
    assert_eq!(revisions["items"][1]["baseAssetId"], first.id.to_string());
    assert_eq!(revisions["items"][1]["hasElementStore"], true);
    assert_eq!(
        owner_get(
            &state,
            &format!("/v1/assets/{}/tileset-revisions", second.id)
        )
        .await,
        revisions
    );

    let first_dir = state.data_dir.join(first.id.to_string());
    assert!(first_dir.join("tileset.json").is_file());
    assert!(store_path(&first_dir).is_file());
    let old_tileset = get_as(
        &state,
        &format!("/api/v1/assets/{}/tileset.json", first.id),
        OWNER,
    )
    .await;
    assert_eq!(old_tileset.status, StatusCode::OK);
    let old_element = owner_get(
        &state,
        &format!("/v1/assets/{}/elements/{REMOVED_WALL}", first.ion_id),
    )
    .await;
    assert_eq!(old_element["label"], "Removed Wall");

    let stranger_upload = upload(&state, STRANGER, &SECOND_REVISION, Some(&first_ion_id)).await;
    assert_eq!(stranger_upload.status, StatusCode::NOT_FOUND);

    // topics belong to the model
    let put = send_json(
        &state,
        "PUT",
        &format!("/v1/assets/{}/bcf/topics/{TOPIC_GUID}", first.ion_id),
        OWNER,
        &minimal_topic(TOPIC_GUID, "Across versions"),
    )
    .await;
    assert_eq!(put.status, StatusCode::CREATED);
    let from_second = owner_get(
        &state,
        &format!("/v1/assets/{}/bcf/topics/{TOPIC_GUID}", second.ion_id),
    )
    .await;
    assert_eq!(from_second["title"], "Across versions");

    let backwards = send_json(
        &state,
        "POST",
        "/v1/assets/compare",
        OWNER,
        &json!({ "fromAssetId": second.ion_id, "toAssetId": first.ion_id }),
    )
    .await;
    assert_eq!(backwards.status, StatusCode::BAD_REQUEST);

    let hidden = send_json(
        &state,
        "POST",
        "/v1/assets/compare",
        STRANGER,
        &json!({ "fromAssetId": first.ion_id, "toAssetId": second.ion_id }),
    )
    .await;
    assert_eq!(hidden.status, StatusCode::NOT_FOUND);

    let created = send_json(
        &state,
        "POST",
        "/v1/assets/compare",
        OWNER,
        &json!({ "fromAssetId": first.ion_id, "toAssetId": second.ion_id }),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.json());
    let metadata = created.json();
    assert_eq!(metadata["type"], "CHANGE_DETECTION");
    let report_id = metadata["id"].as_str().unwrap().to_string();

    let report = wait_for_report(&state, &report_id).await;
    assert_eq!(report["status"], "COMPLETE", "{report}");
    assert_eq!(report["percentComplete"], 100);
    assert_eq!(
        report["summary"],
        json!({ "added": 1, "removed": 1, "changed": 1, "unchanged": 5 })
    );

    let repeated = send_json(
        &state,
        "POST",
        "/v1/assets/compare",
        OWNER,
        &json!({ "fromAssetId": first.ion_id, "toAssetId": second.ion_id }),
    )
    .await;
    assert_eq!(repeated.status, StatusCode::CONFLICT);
    assert!(
        repeated.json()["message"]
            .as_str()
            .unwrap()
            .contains(&report_id),
        "{}",
        repeated.json()
    );

    let changes = owner_get(&state, &format!("/v1/change-reports/{report_id}/changes")).await;
    assert_eq!(changes["items"].as_array().unwrap().len(), 3);

    let added = owner_get(
        &state,
        &format!("/v1/change-reports/{report_id}/changes?kind=added"),
    )
    .await;
    assert_eq!(added["items"][0]["id"], ADDED_WALL);
    assert_eq!(added["items"][0]["label"], "Added Wall");

    let removed = owner_get(
        &state,
        &format!("/v1/change-reports/{report_id}/changes?kind=removed"),
    )
    .await;
    assert_eq!(removed["items"][0]["id"], REMOVED_WALL);

    let changed = owner_get(
        &state,
        &format!("/v1/change-reports/{report_id}/changes?kind=changed"),
    )
    .await;
    let changed_wall = &changed["items"][0];
    assert_eq!(changed_wall["id"], CHANGED_WALL);
    assert_eq!(changed_wall["ifcClass"], "IfcWall");
    assert_eq!(
        changed_wall["changedKeys"],
        json!(["Pset_WallCommon.FireRating"])
    );
    assert_eq!(changed_wall["geometryChanged"], true);
    assert_eq!(changed_wall["locationChanged"], true);

    let page = owner_get(
        &state,
        &format!("/v1/change-reports/{report_id}/changes?top=1"),
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let token_after_first = page["continuationToken"].as_str().unwrap();
    let next_page = owner_get(
        &state,
        &format!(
            "/v1/change-reports/{report_id}/changes?top=2&continuationToken={token_after_first}"
        ),
    )
    .await;
    assert_eq!(next_page["items"].as_array().unwrap().len(), 2);

    let stranger_read = get_as(&state, &format!("/v1/change-reports/{report_id}"), STRANGER).await;
    assert_eq!(stranger_read.status, StatusCode::NOT_FOUND);

    let listed = owner_get(
        &state,
        &format!("/v1/assets/{}/change-reports", second.ion_id),
    )
    .await;
    assert_eq!(listed["items"][0]["id"], report_id.as_str());

    let deleted = send(
        &state,
        Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/assets/{}", first.id))
            .header("authorization", bearer(OWNER, "editor"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(
        get_as(&state, &format!("/v1/change-reports/{report_id}"), OWNER)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let remaining_topic = owner_get(
        &state,
        &format!("/v1/assets/{}/bcf/topics/{TOPIC_GUID}", second.ion_id),
    )
    .await;
    assert_eq!(remaining_topic["title"], "Across versions");
}

#[tokio::test]
async fn a_failed_status_write_ends_the_report_in_error_and_frees_the_pair() {
    let state = test_state().await;
    let first = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let first_ion_id = first.ion_id.to_string();
    let second = upload_and_tile(&state, &SECOND_REVISION, Some(&first_ion_id)).await;
    let compare = json!({ "fromAssetId": first.ion_id, "toAssetId": second.ion_id });
    // sqlite checks a new trigger's name against the connection's cached schema, which misses a drop made on another connection
    let mut schema_connection = state.db.pool.acquire().await.unwrap();

    for (refused_status, reason) in [
        ("IN_PROGRESS", "writing the report's progress failed"),
        ("COMPLETE", "writing the report's result failed"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TRIGGER refuse_status BEFORE UPDATE OF status ON change_reports \
             WHEN NEW.status = '{refused_status}' BEGIN SELECT RAISE(ABORT, 'refused'); END"
        )))
        .execute(&mut *schema_connection)
        .await
        .unwrap();

        let created = send_json(&state, "POST", "/v1/assets/compare", OWNER, &compare).await;
        assert_eq!(created.status, StatusCode::OK, "{}", created.json());
        let report_id = created.json()["id"].as_str().unwrap().to_string();
        let report = wait_for_report(&state, &report_id).await;
        assert_eq!(report["status"], "ERROR", "{report}");
        assert!(
            report["error"].as_str().unwrap().contains(reason),
            "{report}"
        );

        sqlx::query("DROP TRIGGER refuse_status")
            .execute(&mut *schema_connection)
            .await
            .unwrap();
    }

    let created = send_json(&state, "POST", "/v1/assets/compare", OWNER, &compare).await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.json());
    let report = wait_for_report(&state, created.json()["id"].as_str().unwrap()).await;
    assert_eq!(report["status"], "COMPLETE", "{report}");
}

#[tokio::test]
async fn versions_of_the_same_file_report_no_changes() {
    let state = test_state().await;
    let first = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let first_ion_id = first.ion_id.to_string();
    let second = upload_and_tile(&state, &FIRST_REVISION, Some(&first_ion_id)).await;

    let created = send_json(
        &state,
        "POST",
        "/v1/assets/compare",
        OWNER,
        &json!({ "fromAssetId": first.id, "toAssetId": second.id }),
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.json());
    let report_id = created.json()["id"].as_str().unwrap().to_string();
    let report = wait_for_report(&state, &report_id).await;
    assert_eq!(report["status"], "COMPLETE", "{report}");
    assert_eq!(
        report["summary"],
        json!({ "added": 0, "removed": 0, "changed": 0, "unchanged": 7 })
    );
}

#[tokio::test]
async fn a_store_without_geometry_hashes_is_refused_before_a_report_is_queued() {
    use sqlx::{ConnectOptions, Connection};

    let state = test_state().await;
    let first = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let first_ion_id = first.ion_id.to_string();
    let second = upload_and_tile(&state, &SECOND_REVISION, Some(&first_ion_id)).await;
    let mut store = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(store_path(&state.data_dir.join(second.id.to_string())))
        .connect()
        .await
        .unwrap();
    sqlx::query("ALTER TABLE elements DROP COLUMN mesh_hash")
        .execute(&mut store)
        .await
        .unwrap();
    store.close().await.unwrap();

    let answer = send_json(
        &state,
        "POST",
        "/v1/assets/compare",
        OWNER,
        &json!({ "fromAssetId": first.ion_id, "toAssetId": second.ion_id }),
    )
    .await;

    assert_eq!(answer.status, StatusCode::CONFLICT, "{}", answer.json());
    assert_eq!(answer.json()["code"], "ELEMENT_STORE_OUTDATED");
    let message = answer.json()["message"].as_str().unwrap().to_string();
    assert!(message.contains(&second.id.to_string()), "{message}");
    assert!(message.contains("re-tile"), "{message}");
    let reports = owner_get(
        &state,
        &format!("/v1/assets/{}/change-reports", second.ion_id),
    )
    .await;
    assert_eq!(reports["items"], json!([]));
}

#[tokio::test]
async fn assets_that_are_not_versions_of_one_model_cannot_be_compared() {
    let state = test_state().await;
    let first = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let second = upload_and_tile(&state, &SECOND_REVISION, None).await;
    let answer = send_json(
        &state,
        "POST",
        "/v1/assets/compare",
        OWNER,
        &json!({ "fromAssetId": first.ion_id, "toAssetId": second.ion_id }),
    )
    .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    let malformed = send_json(&state, "POST", "/v1/assets/compare", OWNER, &json!({})).await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
}

async fn model_row(state: &Arc<AppState>, status: AssetStatus) -> Uuid {
    let id = Uuid::new_v4();
    state
        .db
        .create_asset(&Asset {
            id,
            name: "version".to_string(),
            asset_type: AssetType::Model,
            status,
            created_at: chrono::Utc::now(),
            tile_count: 1,
            size_bytes: 1,
            description: String::new(),
            tags: Vec::new(),
            owner_id: Some(OWNER.to_string()),
        })
        .await
        .unwrap();
    let asset_dir = state.data_dir.join(id.to_string());
    std::fs::create_dir_all(&asset_dir).unwrap();
    std::fs::write(asset_dir.join("tileset.json"), b"{}").unwrap();
    id
}

#[tokio::test]
async fn retention_keeps_the_newest_five_ready_versions_and_any_unfinished_one() {
    let state = test_state().await;
    let oldest = model_row(&state, AssetStatus::Ready).await;
    let mut ready = vec![oldest];
    for _ in 0..6 {
        let next = model_row(&state, AssetStatus::Ready).await;
        model_versions::add_version(&state.db, next, oldest)
            .await
            .unwrap();
        ready.push(next);
    }
    let still_uploading = model_row(&state, AssetStatus::Uploading).await;
    model_versions::add_version(&state.db, still_uploading, oldest)
        .await
        .unwrap();

    model_versions::apply_retention(&state.db, &state.data_dir, &state.webhooks, still_uploading)
        .await
        .unwrap();

    for removed in &ready[..2] {
        assert!(state.db.get_asset(*removed).await.unwrap().is_none());
        assert!(!state.data_dir.join(removed.to_string()).exists());
    }
    for kept in ready[2..].iter().chain([&still_uploading]) {
        assert!(state.db.get_asset(*kept).await.unwrap().is_some());
        assert!(
            state
                .data_dir
                .join(kept.to_string())
                .join("tileset.json")
                .is_file()
        );
    }
    assert_eq!(
        model_versions::versions_newest_first(&state.db, oldest)
            .await
            .unwrap()
            .len(),
        6
    );
}

#[tokio::test]
async fn retention_removes_older_versions_past_the_byte_limit_and_never_the_newest() {
    let state = test_state().await;
    let older = model_row(&state, AssetStatus::Ready).await;
    let newest = model_row(&state, AssetStatus::Ready).await;
    model_versions::add_version(&state.db, newest, older)
        .await
        .unwrap();
    // a sparse file has a length past the limit and takes no disk blocks
    let sparse = std::fs::File::create(state.data_dir.join(older.to_string()).join("big")).unwrap();
    sparse.set_len(21 * 1024 * 1024 * 1024).unwrap();

    model_versions::apply_retention(&state.db, &state.data_dir, &state.webhooks, newest)
        .await
        .unwrap();

    assert!(state.db.get_asset(older).await.unwrap().is_none());
    assert!(state.db.get_asset(newest).await.unwrap().is_some());
}

#[tokio::test]
async fn deleting_the_last_version_deletes_the_topics_and_their_snapshots() {
    let state = test_state().await;
    let model = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let answer = import_bcf(&state, model.ion_id, OWNER, &full_bcf()).await;
    assert_eq!(answer.status, StatusCode::OK);
    let snapshots = state
        .data_dir
        .join("bcf_snapshots")
        .join(model.id.to_string());
    assert!(snapshots.is_dir());

    let deleted = send(
        &state,
        Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/assets/{}", model.id))
            .header("authorization", bearer(OWNER, "editor"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert!(!snapshots.exists());
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM bcf_topics")
        .fetch_one(&state.db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn an_ion_create_with_base_asset_id_adds_a_version() {
    let state = test_state().await;
    let first = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let body = json!({
        "name": "second",
        "type": "3DTILES",
        "options": {
            "sourceType": "BIM_CAD",
            "baseAssetId": first.ion_id,
            "existingFileBehavior": "KEEP",
        },
    });
    let created = send_json(&state, "POST", "/v1/assets", OWNER, &body).await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.json());
    let new_ion_id = created.json()["assetMetadata"]["id"].clone();

    let revisions = owner_get(
        &state,
        &format!("/v1/assets/{}/tileset-revisions", first.ion_id),
    )
    .await;
    assert_eq!(revisions["total"], 2);
    assert_eq!(revisions["items"][0]["assetId"], new_ion_id);
    assert_eq!(revisions["items"][0]["status"], "uploading");

    let stranger = send_json(&state, "POST", "/v1/assets", STRANGER, &body).await;
    assert_eq!(stranger.status, StatusCode::NOT_FOUND);

    let no_behavior = json!({
        "name": "third",
        "type": "3DTILES",
        "options": { "sourceType": "BIM_CAD", "baseAssetId": first.ion_id },
    });
    let refused = send_json(&state, "POST", "/v1/assets", OWNER, &no_behavior).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);

    let wrong_source = json!({
        "name": "fourth",
        "type": "3DTILES",
        "options": {
            "sourceType": "3D_MODEL",
            "baseAssetId": first.ion_id,
            "existingFileBehavior": "KEEP",
        },
    });
    let refused = send_json(&state, "POST", "/v1/assets", OWNER, &wrong_source).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_version_of_an_ownerless_model_has_no_owner() {
    let state = test_state().await;
    let base_id = Uuid::new_v4();
    let base_ion_id = state
        .db
        .create_asset(&Asset {
            id: base_id,
            name: "ownerless".to_string(),
            asset_type: AssetType::Model,
            status: AssetStatus::Ready,
            created_at: chrono::Utc::now(),
            tile_count: 1,
            size_bytes: 1,
            description: String::new(),
            tags: Vec::new(),
            owner_id: None,
        })
        .await
        .unwrap();
    let body = json!({
        "name": "second",
        "type": "3DTILES",
        "options": {
            "sourceType": "BIM_CAD",
            "baseAssetId": base_ion_id,
            "existingFileBehavior": "KEEP",
        },
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/assets")
        .header("authorization", bearer("an-admin", "admin"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let created = send(&state, request).await;

    assert_eq!(created.status, StatusCode::OK, "{}", created.json());
    let version_ion_id = created.json()["assetMetadata"]["id"].as_i64().unwrap();
    let (version, _) = state
        .db
        .get_asset_by_ion_id(version_ion_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(version.owner_id, None);
}

struct Extents {
    minimum: [f64; 3],
    maximum: [f64; 3],
    triangles: usize,
}

fn extents_of(meshes: &[tiletopia_ingest::MeshData]) -> Extents {
    let mut extents = Extents {
        minimum: [f64::INFINITY; 3],
        maximum: [f64::NEG_INFINITY; 3],
        triangles: 0,
    };
    for mesh in meshes {
        extents.triangles += mesh.indices.len() / 3;
        for index in &mesh.indices {
            let position = mesh.positions[*index as usize];
            for (axis, coordinate) in position.into_iter().enumerate() {
                extents.minimum[axis] = extents.minimum[axis].min(coordinate);
                extents.maximum[axis] = extents.maximum[axis].max(coordinate);
            }
        }
    }
    extents
}

fn read_dxf_back(bytes: &[u8]) -> Extents {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("exported.dxf");
    std::fs::write(&path, bytes).unwrap();
    let meshes = tiletopia_ingest::dxf_reader::read(&path, u64::MAX, None).unwrap();
    extents_of(&meshes)
}

#[tokio::test]
async fn a_dxf_export_reads_back_with_the_source_meshes_and_a_cut_stays_below_its_height() {
    let state = test_state().await;
    let model = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let source = tiletopia_ingest::read_mesh(&model.input_path, u64::MAX).unwrap();
    let source_extents = extents_of(&source);
    assert!(source_extents.triangles > 0);

    let whole = get_as(&state, &format!("/v1/assets/{}/dxf", model.ion_id), OWNER).await;
    assert_eq!(whole.status, StatusCode::OK, "{}", whole.json());
    let text = std::str::from_utf8(&whole.bytes).unwrap();
    assert!(text.contains("\n9\n$ACADVER\n1\nAC1015\n"));
    assert!(text.contains("\n9\n$INSUNITS\n70\n6\n"));
    let lines: Vec<&str> = text.lines().collect();
    let groups: Vec<(&str, &str)> = lines.chunks(2).map(|pair| (pair[0], pair[1])).collect();
    let header_end = groups
        .iter()
        .position(|&group| group == ("0", "ENDSEC"))
        .unwrap();
    let handles: Vec<&str> = groups[header_end..]
        .iter()
        .filter(|(code, _)| *code == "5" || *code == "105")
        .map(|(_, handle)| *handle)
        .collect();
    let faces = groups
        .iter()
        .filter(|&&group| group == ("0", "3DFACE"))
        .count();
    assert!(faces > 0);
    assert!(handles.len() > faces);
    let unique: std::collections::HashSet<&str> = handles.iter().copied().collect();
    assert_eq!(unique.len(), handles.len());
    let exported = read_dxf_back(&whole.bytes);
    assert_eq!(exported.triangles, source_extents.triangles);
    for axis in 0..3 {
        assert!((exported.minimum[axis] - source_extents.minimum[axis]).abs() < 1e-9);
        assert!((exported.maximum[axis] - source_extents.maximum[axis]).abs() < 1e-9);
    }

    let cut_height = (source_extents.minimum[2] + source_extents.maximum[2]) / 2.0;
    let cut = get_as(
        &state,
        &format!("/v1/assets/{}/dxf?cutHeight={cut_height}", model.ion_id),
        OWNER,
    )
    .await;
    assert_eq!(cut.status, StatusCode::OK, "{}", cut.json());
    let below = read_dxf_back(&cut.bytes);
    assert!((below.maximum[2] - cut_height).abs() < 1e-9);
    assert!((below.minimum[2] - source_extents.minimum[2]).abs() < 1e-9);

    let nothing_below = get_as(
        &state,
        &format!(
            "/v1/assets/{}/dxf?cutHeight={}",
            model.ion_id,
            source_extents.minimum[2] - 1.0
        ),
        OWNER,
    )
    .await;
    assert_eq!(nothing_below.status, StatusCode::BAD_REQUEST);
    let not_a_number = get_as(
        &state,
        &format!("/v1/assets/{}/dxf?cutHeight=NaN", model.ion_id),
        OWNER,
    )
    .await;
    assert_eq!(not_a_number.status, StatusCode::BAD_REQUEST);
    assert!(scratch_is_empty(&state));
}

#[tokio::test]
async fn every_route_needs_a_bearer_token_from_someone_who_may_view_the_asset() {
    let state = test_state().await;
    let model = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let ion_id = model.ion_id;
    let read_uris = [
        format!("/v1/assets/{ion_id}/tileset-revisions"),
        format!("/v1/assets/{ion_id}/change-reports"),
        format!("/v1/assets/{ion_id}/dxf"),
        format!("/v1/assets/{ion_id}/section"),
        format!("/v1/assets/{ion_id}/bcf/export"),
        format!("/v1/assets/{ion_id}/bcf/topics"),
        format!("/v1/assets/{ion_id}/bcf/topics/{TOPIC_GUID}"),
        format!("/v1/assets/{ion_id}/bcf/topics/{TOPIC_GUID}/viewpoints/{VIEWPOINT_GUID}/snapshot"),
        format!("/v1/assets/{ion_id}/element-edits"),
        format!("/v1/assets/{ion_id}/elements/{KEPT_WALL}/volume"),
    ];
    for uri in &read_uris {
        let stranger = get_as(&state, uri, STRANGER).await;
        assert_eq!(stranger.status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(stranger.json()["code"], "ResourceNotFound", "{uri}");

        let anonymous = send(
            &state,
            Request::builder().uri(uri).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED, "{uri}");

        let query_token = send(
            &state,
            Request::builder()
                .uri(format!("{uri}?access_token={}", token(OWNER, "viewer")))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(query_token.status, StatusCode::UNAUTHORIZED, "{uri}");
    }

    let stranger_import = import_bcf(&state, ion_id, STRANGER, &full_bcf()).await;
    assert_eq!(stranger_import.status, StatusCode::NOT_FOUND);

    let stranger_put = send_json(
        &state,
        "PUT",
        &format!("/v1/assets/{ion_id}/bcf/topics/{TOPIC_GUID}"),
        STRANGER,
        &minimal_topic(TOPIC_GUID, "Hidden"),
    )
    .await;
    assert_eq!(stranger_put.status, StatusCode::NOT_FOUND);

    let stranger_delete = send_json(
        &state,
        "DELETE",
        &format!("/v1/assets/{ion_id}/bcf/topics/{TOPIC_GUID}"),
        STRANGER,
        &json!({}),
    )
    .await;
    assert_eq!(stranger_delete.status, StatusCode::NOT_FOUND);

    let stranger_edit = send_json(
        &state,
        "POST",
        &format!("/v1/assets/{ion_id}/edits"),
        STRANGER,
        &json!({ "kind": "delete", "elementId": KEPT_WALL }),
    )
    .await;
    assert_eq!(stranger_edit.status, StatusCode::NOT_FOUND);
    assert!(
        std::fs::read_to_string(&model.input_path)
            .unwrap()
            .contains(KEPT_WALL)
    );

    let topics = owner_get(&state, &format!("/v1/assets/{ion_id}/bcf/topics")).await;
    assert_eq!(topics["total"], 0);
}

struct WriteRoute {
    method: &'static str,
    uri: String,
    content_type: String,
    body: Vec<u8>,
    success: StatusCode,
}

fn owner_write(route: &WriteRoute, role: &str) -> Request<Body> {
    Request::builder()
        .method(route.method)
        .uri(route.uri.as_str())
        .header("authorization", bearer(OWNER, role))
        .header("content-type", route.content_type.as_str())
        .body(Body::from(route.body.clone()))
        .unwrap()
}

#[tokio::test]
async fn an_owner_with_the_viewer_role_is_refused_every_write_an_editor_may_make() {
    let state = test_state().await;
    let first = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let first_ion_id = first.ion_id.to_string();
    let second = upload_and_tile(&state, &SECOND_REVISION, Some(&first_ion_id)).await;

    let multipart = format!("multipart/form-data; boundary={BOUNDARY}");
    let json_type = "application/json".to_string();
    let topic_uri = format!("/v1/assets/{}/bcf/topics/{TOPIC_GUID}", first.ion_id);
    let routes = [
        WriteRoute {
            method: "POST",
            uri: format!("/v1/assets/{}/bcf/import", first.ion_id),
            content_type: multipart.clone(),
            body: multipart_body(&[], "issues.bcf", &full_bcf()),
            success: StatusCode::OK,
        },
        WriteRoute {
            method: "PUT",
            uri: topic_uri.clone(),
            content_type: json_type.clone(),
            body: minimal_topic(TOPIC_GUID, "Edited").to_string().into_bytes(),
            success: StatusCode::OK,
        },
        WriteRoute {
            method: "DELETE",
            uri: topic_uri,
            content_type: json_type.clone(),
            body: Vec::new(),
            success: StatusCode::NO_CONTENT,
        },
        WriteRoute {
            method: "POST",
            uri: "/v1/assets/compare".to_string(),
            content_type: json_type.clone(),
            body: json!({ "fromAssetId": first.ion_id, "toAssetId": second.ion_id })
                .to_string()
                .into_bytes(),
            success: StatusCode::OK,
        },
        WriteRoute {
            method: "POST",
            uri: "/api/v1/assets".to_string(),
            content_type: multipart,
            body: multipart_body(
                &[
                    ("longitude", "10"),
                    ("latitude", "20"),
                    ("base_asset_id", first_ion_id.as_str()),
                ],
                "model.ifc",
                model_ifc(&SECOND_REVISION).as_bytes(),
            ),
            success: StatusCode::CREATED,
        },
        WriteRoute {
            method: "POST",
            uri: "/v1/assets".to_string(),
            content_type: json_type,
            body: json!({
                "name": "third",
                "type": "3DTILES",
                "options": {
                    "sourceType": "BIM_CAD",
                    "baseAssetId": first.ion_id,
                    "existingFileBehavior": "KEEP",
                },
            })
            .to_string()
            .into_bytes(),
            success: StatusCode::OK,
        },
        WriteRoute {
            method: "POST",
            uri: format!("/v1/assets/{}/edits", first.ion_id),
            content_type: "application/json".to_string(),
            body: json!({ "kind": "delete", "elementId": REMOVED_WALL })
                .to_string()
                .into_bytes(),
            success: StatusCode::OK,
        },
    ];
    for route in &routes {
        let viewer = send(&state, owner_write(route, "viewer")).await;
        assert_eq!(
            viewer.status,
            StatusCode::FORBIDDEN,
            "{} {}",
            route.method,
            route.uri
        );
        let editor = send(&state, owner_write(route, "editor")).await;
        assert_eq!(
            editor.status,
            route.success,
            "{} {}: {}",
            route.method,
            route.uri,
            editor.json()
        );
    }
}

#[tokio::test]
async fn a_model_with_no_element_store_refuses_a_bcf_import() {
    let state = test_state().await;
    let id = model_row(&state, AssetStatus::Ready).await;
    let (_, ion_id) = state.db.get_asset_with_ion_id(id).await.unwrap().unwrap();
    let answer = import_bcf(&state, ion_id, OWNER, &full_bcf()).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(answer.json()["code"], "ASSET_HAS_NO_ELEMENT_STORE");
}

async fn post_edit(state: &Arc<AppState>, ion_id: i64, edit: Value) -> Answer {
    send_json(
        state,
        "POST",
        &format!("/v1/assets/{ion_id}/edits"),
        OWNER,
        &edit,
    )
    .await
}

async fn put_fire_rating(state: &Arc<AppState>, ion_id: i64, rating: &str) -> Answer {
    send_json(
        state,
        "PUT",
        &format!("/v1/assets/{ion_id}/elements/{CHANGED_WALL}/properties"),
        OWNER,
        &json!({ "key": "Pset_WallCommon.FireRating", "value": rating }),
    )
    .await
}

async fn parent_path(state: &Arc<AppState>, ion_id: i64, element_id: &str) -> Vec<String> {
    let parents = owner_get(
        state,
        &format!("/v1/assets/{ion_id}/tree/parents?elementId={element_id}"),
    )
    .await;
    parents["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn edits_move_delete_and_relate_elements_and_the_volume_reads_the_mesh() {
    let state = test_state().await;
    let model = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let ion_id = model.ion_id;

    let volume = owner_get(
        &state,
        &format!("/v1/assets/{ion_id}/elements/{CHANGED_WALL}/volume"),
    )
    .await;
    assert!(
        (volume["volume"].as_f64().unwrap() - 6.0).abs() < 1.0e-6,
        "{volume}"
    );
    let unknown = get_as(
        &state,
        &format!("/v1/assets/{ion_id}/elements/{UNKNOWN_ELEMENT}/volume"),
        OWNER,
    )
    .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.json()["code"], "ELEMENT_HAS_NO_MESH");

    let empty = owner_get(&state, &format!("/v1/assets/{ion_id}/element-edits")).await;
    assert_eq!(empty["edits"], json!([]));

    let moved = post_edit(
        &state,
        ion_id,
        json!({ "kind": "translate", "elementId": KEPT_WALL, "delta": [1.0, 0.0, 0.0] }),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.json());
    assert_eq!(moved.json()["drawn"], true);
    let source = std::fs::read_to_string(&model.input_path).unwrap();
    assert!(
        source.contains("IFCCARTESIANPOINT((1.0,0.0,0.0))"),
        "{source}"
    );

    let deleted = post_edit(
        &state,
        ion_id,
        json!({ "kind": "delete", "elementId": REMOVED_WALL }),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.json());
    let gone = get_as(
        &state,
        &format!("/v1/assets/{ion_id}/elements/{REMOVED_WALL}"),
        OWNER,
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);

    let edits = owner_get(&state, &format!("/v1/assets/{ion_id}/element-edits")).await;
    let edits = edits["edits"].as_array().unwrap();
    assert_eq!(edits.len(), 2, "{edits:?}");
    assert_eq!(edits[0]["elementId"], KEPT_WALL);
    assert_eq!(edits[0]["kind"], "translate");
    assert_eq!(edits[0]["hideOriginal"], true);
    assert!(!edits[0]["positions"].as_array().unwrap().is_empty());
    assert_eq!(edits[1]["elementId"], REMOVED_WALL);
    assert_eq!(edits[1]["kind"], "delete");

    let contained = [PROJECT, SITE, BUILDING, STOREY, CHANGED_WALL];
    assert_eq!(parent_path(&state, ion_id, CHANGED_WALL).await, contained);
    let twice = post_edit(
        &state,
        ion_id,
        json!({ "kind": "relation", "elementId": CHANGED_WALL, "relationId": 43, "add": true }),
    )
    .await;
    assert_eq!(twice.status, StatusCode::CONFLICT);
    assert_eq!(twice.json()["code"], "ALREADY_RELATED");

    let removed = post_edit(
        &state,
        ion_id,
        json!({ "kind": "relation", "elementId": CHANGED_WALL, "relationId": 43, "add": false }),
    )
    .await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.json());
    assert_eq!(
        parent_path(&state, ion_id, CHANGED_WALL).await,
        [PROJECT, CHANGED_WALL]
    );
    let restored = post_edit(
        &state,
        ion_id,
        json!({ "kind": "relation", "elementId": CHANGED_WALL, "relationId": 43, "add": true }),
    )
    .await;
    assert_eq!(restored.status, StatusCode::OK, "{}", restored.json());
    assert_eq!(parent_path(&state, ion_id, CHANGED_WALL).await, contained);
}

#[tokio::test]
async fn an_unreadable_edit_file_answers_500_and_is_kept() {
    let state = test_state().await;
    let model = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let edits_path = state
        .data_dir
        .join(model.id.to_string())
        .join("element-edits.json");
    std::fs::write(&edits_path, b"{ not json").unwrap();
    let source = std::fs::read(&model.input_path).unwrap();

    let listed = get_as(
        &state,
        &format!("/v1/assets/{}/element-edits", model.ion_id),
        OWNER,
    )
    .await;
    assert_eq!(listed.status, StatusCode::INTERNAL_SERVER_ERROR);

    let moved = post_edit(
        &state,
        model.ion_id,
        json!({ "kind": "translate", "elementId": KEPT_WALL, "delta": [1.0, 0.0, 0.0] }),
    )
    .await;
    assert_eq!(moved.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(std::fs::read(&edits_path).unwrap(), b"{ not json");
    assert_eq!(std::fs::read(&model.input_path).unwrap(), source);
}

#[tokio::test]
async fn writes_to_one_model_wait_for_each_other_and_both_land() {
    let state = test_state().await;
    let model = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let ion_id = model.ion_id;

    let other_write = tiletopia_server::edits::lock_asset_writes(model.id).await;
    let edit = tokio::spawn({
        let state = Arc::clone(&state);
        async move {
            post_edit(
                &state,
                ion_id,
                json!({ "kind": "translate", "elementId": KEPT_WALL, "delta": [1.0, 0.0, 0.0] }),
            )
            .await
        }
    });
    let property = tokio::spawn({
        let state = Arc::clone(&state);
        async move { put_fire_rating(&state, ion_id, "REI99").await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!edit.is_finished());
    assert!(!property.is_finished());

    drop(other_write);
    let edit = edit.await.unwrap();
    assert_eq!(edit.status, StatusCode::OK, "{}", edit.json());
    let property = property.await.unwrap();
    assert_eq!(property.status, StatusCode::OK, "{}", property.json());
    let source = std::fs::read_to_string(&model.input_path).unwrap();
    assert!(source.contains("IFCLABEL('REI99')"), "{source}");
    assert!(
        source.contains("IFCCARTESIANPOINT((1.0,0.0,0.0))"),
        "{source}"
    );
}

#[tokio::test]
async fn an_edit_waiting_while_a_re_tile_starts_is_refused_and_writes_nothing() {
    let state = test_state().await;
    let model = upload_and_tile(&state, &FIRST_REVISION, None).await;
    let ion_id = model.ion_id;
    let before = std::fs::read(&model.input_path).unwrap();

    let job_start = tiletopia_server::edits::lock_asset_writes(model.id).await;
    let edit = tokio::spawn({
        let state = Arc::clone(&state);
        async move {
            post_edit(
                &state,
                ion_id,
                json!({ "kind": "translate", "elementId": KEPT_WALL, "delta": [1.0, 0.0, 0.0] }),
            )
            .await
        }
    });
    let property = tokio::spawn({
        let state = Arc::clone(&state);
        async move { put_fire_rating(&state, ion_id, "REI99").await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let mut asset = state.db.get_asset(model.id).await.unwrap().unwrap();
    asset.status = tiletopia_server::AssetStatus::Tiling;
    state.db.update_asset(&asset).await.unwrap();
    drop(job_start);

    for answer in [edit.await.unwrap(), property.await.unwrap()] {
        assert_eq!(answer.status, StatusCode::CONFLICT, "{}", answer.json());
        assert_eq!(answer.json()["code"], "ASSET_NOT_READY");
    }
    assert_eq!(std::fs::read(&model.input_path).unwrap(), before);
}

#[tokio::test]
async fn reads_and_writes_of_the_whole_model_wait_for_a_build_slot() {
    let state = test_state().await;
    let ion_id = upload_and_tile(&state, &FIRST_REVISION, None).await.ion_id;
    let other_ion_id = upload_and_tile(&state, &FIRST_REVISION, None).await.ion_id;

    let mut held_slots = Vec::new();
    while let Ok(slot) = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        state.elevation_store.build_slot(),
    )
    .await
    {
        held_slots.push(slot);
    }
    assert!(!held_slots.is_empty());

    let spawn_get = |uri: String| {
        let state = Arc::clone(&state);
        tokio::spawn(async move { get_as(&state, &uri, OWNER).await })
    };
    let volume = spawn_get(format!(
        "/v1/assets/{ion_id}/elements/{CHANGED_WALL}/volume"
    ));
    let alignments = spawn_get(format!("/v1/assets/{ion_id}/alignments"));
    let edit = tokio::spawn({
        let state = Arc::clone(&state);
        async move {
            post_edit(
                &state,
                ion_id,
                json!({ "kind": "delete", "elementId": REMOVED_WALL }),
            )
            .await
        }
    });
    let property = tokio::spawn({
        let state = Arc::clone(&state);
        async move { put_fire_rating(&state, other_ion_id, "REI99").await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let requests = [
        ("volume", volume),
        ("alignments", alignments),
        ("edit", edit),
        ("property", property),
    ];
    for (name, request) in &requests {
        assert!(!request.is_finished(), "{name}");
    }

    drop(held_slots);
    for (name, request) in requests {
        let answer = request.await.unwrap();
        assert_eq!(answer.status, StatusCode::OK, "{name}: {}", answer.json());
    }
}

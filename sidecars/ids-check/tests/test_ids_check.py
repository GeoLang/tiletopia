import json
import shutil
import threading
import urllib.error
import urllib.request
from pathlib import Path

import pytest

from ids_check import TOKEN_ENV, CheckServer, required_token

FIXTURES = Path(__file__).parent / "fixtures"
RATED_WALL = "2O2Fr$t4X7Zf8NOew3FLOH"
UNRATED_WALL = "1hOSvn6df7F8_7GcBWlS_W"
WALLS = {"ifc_path": "asset/walls.ifc", "ids_path": "asset/wall_fire_rating.ids"}
TOKEN = "sidecar-test-token"


@pytest.fixture
def data_dir(tmp_path):
    data = tmp_path / "data"
    (data / "asset").mkdir(parents=True)
    shutil.copy(FIXTURES / "walls.ifc", data / "asset" / "walls.ifc")
    shutil.copy(FIXTURES / "wall_fire_rating.ids", data / "asset" / "wall_fire_rating.ids")
    return data


@pytest.fixture
def sidecar_url(data_dir):
    server = CheckServer(("127.0.0.1", 0), data_dir, TOKEN)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    yield f"http://127.0.0.1:{server.server_address[1]}"
    server.shutdown()
    server.server_close()


def post_check(sidecar_url, body, authorization=f"Bearer {TOKEN}"):
    headers = {"Content-Type": "application/json"}
    if authorization is not None:
        headers["Authorization"] = authorization
    request = urllib.request.Request(
        f"{sidecar_url}/check",
        data=json.dumps(body).encode("utf-8"),
        headers=headers,
        method="POST",
    )
    try:
        with urllib.request.urlopen(request) as response:
            return response.status, json.loads(response.read())
    except urllib.error.HTTPError as error:
        return error.code, json.loads(error.read())


def test_health_answers_200(sidecar_url):
    with urllib.request.urlopen(f"{sidecar_url}/health") as response:
        assert response.status == 200


def test_a_check_reports_the_wall_without_a_fire_rating(sidecar_url):
    status, report = post_check(sidecar_url, WALLS)

    assert status == 200, report
    assert report["status"] is False
    assert report["filepath"] == "asset/walls.ifc"
    requirement = report["specifications"][0]["requirements"][0]
    assert [entity["global_id"] for entity in requirement["failed_entities"]] == [UNRATED_WALL]
    assert [entity["global_id"] for entity in requirement["passed_entities"]] == [RATED_WALL]


def test_a_path_leaving_the_data_directory_is_refused(sidecar_url, data_dir):
    shutil.copy(FIXTURES / "walls.ifc", data_dir.parent / "outside.ifc")

    status, answer = post_check(sidecar_url, {**WALLS, "ifc_path": "asset/../../outside.ifc"})

    assert status == 400
    assert "leaves the data directory" in answer["error"]


def test_an_absolute_path_is_refused(sidecar_url, data_dir):
    status, answer = post_check(sidecar_url, {**WALLS, "ids_path": str(data_dir / WALLS["ids_path"])})

    assert status == 400
    assert "relative" in answer["error"]


def test_an_invalid_ids_file_answers_422(sidecar_url, data_dir):
    (data_dir / "asset" / "broken.ids").write_text('<ids xmlns="http://standards.buildingsmart.org/IDS"><info/></ids>')

    status, answer = post_check(sidecar_url, {**WALLS, "ids_path": "asset/broken.ids"})

    assert status == 422
    assert "IDS" in answer["error"]


@pytest.mark.parametrize("authorization", [None, "Bearer another-token", TOKEN])
def test_a_check_without_the_matching_bearer_token_is_refused(sidecar_url, authorization):
    status, answer = post_check(sidecar_url, WALLS, authorization=authorization)

    assert status == 401
    assert TOKEN_ENV in answer["error"]
    assert "specifications" not in answer


def test_the_server_does_not_start_without_a_token():
    with pytest.raises(SystemExit, match=TOKEN_ENV):
        required_token({TOKEN_ENV: " "})
    with pytest.raises(SystemExit, match=TOKEN_ENV):
        required_token({})
    assert required_token({TOKEN_ENV: TOKEN}) == TOKEN

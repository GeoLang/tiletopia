import hmac
import json
import logging
import os
from collections.abc import Mapping
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import ifcopenshell
import xmlschema
from ifctester import ids, reporter


class Refusal(Exception):
    def __init__(self, status: HTTPStatus, message: str):
        super().__init__(message)
        self.status = status


def path_in_data_dir(data_dir: Path, relative: object, field: str) -> Path:
    if not isinstance(relative, str) or not relative:
        raise Refusal(HTTPStatus.BAD_REQUEST, f"{field} must be a path relative to the data directory")
    if Path(relative).is_absolute():
        raise Refusal(HTTPStatus.BAD_REQUEST, f"{field} {relative!r} is absolute, it must be relative to the data directory")
    resolved = (data_dir / relative).resolve()
    if not resolved.is_relative_to(data_dir.resolve()):
        raise Refusal(HTTPStatus.BAD_REQUEST, f"{field} {relative!r} leaves the data directory")
    if not resolved.is_file():
        raise Refusal(HTTPStatus.NOT_FOUND, f"{field} {relative!r} is not a file in the data directory")
    return resolved


def checked_report(data_dir: Path, request: dict) -> str:
    ifc_path = path_in_data_dir(data_dir, request.get("ifc_path"), "ifc_path")
    ids_path = path_in_data_dir(data_dir, request.get("ids_path"), "ids_path")
    try:
        specification = ids.open(str(ids_path), validate=True)
    except ids.IdsXmlValidationError as error:
        reason = error.xml_error.reason or error.xml_error.message
        raise Refusal(HTTPStatus.UNPROCESSABLE_ENTITY, f"the IDS file is not valid IDS: {reason}")
    except xmlschema.XMLSchemaException as error:
        raise Refusal(HTTPStatus.UNPROCESSABLE_ENTITY, f"the IDS file could not be read: {error}")
    try:
        model = ifcopenshell.open(str(ifc_path))
    except (ifcopenshell.Error, OSError) as error:
        raise Refusal(HTTPStatus.UNPROCESSABLE_ENTITY, f"the IFC file could not be read: {error}")
    specification.validate(model, filepath=request["ifc_path"])
    json_reporter = reporter.Json(specification)
    json_reporter.report()
    return json_reporter.to_string()


MOST_REQUEST_BYTES = 64 * 1024
TOKEN_ENV = "IDS_CHECK_TOKEN"


class CheckHandler(BaseHTTPRequestHandler):
    server: "CheckServer"

    def answer(self, status: HTTPStatus, body: str) -> None:
        encoded = body.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def refuse(self, status: HTTPStatus, message: str) -> None:
        self.answer(status, json.dumps({"error": message}))

    def read_request(self) -> dict:
        length = self.headers.get("Content-Length")
        if length is None:
            raise Refusal(HTTPStatus.LENGTH_REQUIRED, "the request needs a Content-Length")
        if not length.isdigit():
            raise Refusal(HTTPStatus.BAD_REQUEST, f"Content-Length {length!r} is not a byte count")
        if int(length) > MOST_REQUEST_BYTES:
            raise Refusal(HTTPStatus.REQUEST_ENTITY_TOO_LARGE, f"the request is over {MOST_REQUEST_BYTES} bytes")
        try:
            request = json.loads(self.rfile.read(int(length)))
        except json.JSONDecodeError as error:
            raise Refusal(HTTPStatus.BAD_REQUEST, f"the request is not JSON: {error}")
        if not isinstance(request, dict):
            raise Refusal(HTTPStatus.BAD_REQUEST, "the request must be a JSON object")
        return request

    def do_GET(self) -> None:
        if self.path != "/health":
            self.refuse(HTTPStatus.NOT_FOUND, f"no route GET {self.path}")
            return
        self.answer(HTTPStatus.OK, json.dumps({"status": "ok"}))

    def do_POST(self) -> None:
        if self.path != "/check":
            self.refuse(HTTPStatus.NOT_FOUND, f"no route POST {self.path}")
            return
        authorization = self.headers.get("Authorization", "")
        if not hmac.compare_digest(authorization.encode("utf-8"), f"Bearer {self.server.token}".encode("utf-8")):
            self.refuse(HTTPStatus.UNAUTHORIZED, f"the request has no Authorization: Bearer header matching {TOKEN_ENV}")
            return
        try:
            report = checked_report(self.server.data_dir, self.read_request())
        except Refusal as refusal:
            self.refuse(refusal.status, str(refusal))
            return
        # http.server drops the connection on an uncaught exception, and the caller needs an error body
        except Exception as error:
            logging.exception("the IDS check failed")
            self.refuse(HTTPStatus.INTERNAL_SERVER_ERROR, f"the IDS check failed: {error}")
            return
        self.answer(HTTPStatus.OK, report)


class CheckServer(ThreadingHTTPServer):
    def __init__(self, address: tuple[str, int], data_dir: Path, token: str):
        super().__init__(address, CheckHandler)
        self.data_dir = data_dir
        self.token = token


def required_token(environment: Mapping[str, str]) -> str:
    token = environment.get(TOKEN_ENV, "").strip()
    if not token:
        raise SystemExit(f"{TOKEN_ENV} must be set to the token tiletopia sends as TILETOPIA_IDS_CHECK_TOKEN")
    return token


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    CheckServer(("0.0.0.0", 8000), Path("/data"), required_token(os.environ)).serve_forever()

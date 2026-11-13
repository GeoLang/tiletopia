#!/usr/bin/env bash
# usage: scripts/validate-tiles.sh <compression>
set -euo pipefail
# bash passes an unmatched glob on as literal text, and point cloud tilesets have no .glb tiles
shopt -s nullglob

compression=${1:?usage: validate-tiles.sh <compression>}
repo_root=$(cd "$(dirname "$0")/.." && pwd)
validator_dir=$repo_root/scripts/tiles-validator
port=38473
api=http://127.0.0.1:$port/api/v1
email=validator@example.com
password=validator-password

pnpm --dir "$validator_dir" install --frozen-lockfile
cargo build --manifest-path "$repo_root/Cargo.toml" --bin tiletopia
tiletopia=${CARGO_TARGET_DIR:-$repo_root/target}/debug/tiletopia

work_dir=$(mktemp -d)
data_dir=$work_dir/data
node "$validator_dir/make-fixtures.mjs" "$work_dir"

TILETOPIA_ION_BASE_URL=http://127.0.0.1:$port TILETOPIA_JWT_SECRET=$(openssl rand -hex 32) \
  "$tiletopia" serve --data-dir "$data_dir" --host 127.0.0.1 --port "$port" &
server_pid=$!
trap 'kill "$server_pid"; rm -rf "$work_dir"' EXIT

curl -fsS --retry 30 --retry-delay 1 --retry-connrefused "$api/health" >/dev/null

credentials=$(jq -n --arg email "$email" --arg password "$password" \
  '{email: $email, password: $password, name: "validator"}')
curl -fsS -H 'Content-Type: application/json' -d "$credentials" "$api/auth/signup" >/dev/null
"$tiletopia" set-role "$email" editor --data-dir "$data_dir"
token=$(curl -fsS -H 'Content-Type: application/json' -d "$credentials" "$api/auth/login" | jq -er .token)

tile_and_validate() {
  local fixture=$1
  shift
  local response asset_id job_id status report
  response=$(curl -fsS -H "Authorization: Bearer $token" -F "file=@$fixture" \
    -F "mesh_compression=$compression" "$@" "$api/assets")
  asset_id=$(jq -er .id <<<"$response")
  job_id=$(jq -er .job_id <<<"$response")

  status=queued
  for _ in $(seq 600); do
    status=$(curl -fsS -H "Authorization: Bearer $token" "$api/jobs/$job_id" | jq -er .status)
    if [ "$status" != queued ] && [ "$status" != running ]; then
      break
    fi
    sleep 1
  done
  if [ "$status" != done ]; then
    echo "$fixture: tiling job ended as $status" >&2
    return 1
  fi

  if [ "$compression" != none ]; then
    for tile in "$data_dir/$asset_id"/tiles/*.glb; do
      node -e 'const glb = require("fs").readFileSync(process.argv[1]); console.log(glb.subarray(20, 20 + glb.readUInt32LE(12)).toString())' "$tile" |
        jq -e '(.extensionsUsed // []) | any(test("compression"))' >/dev/null || {
        echo "$tile: no glTF compression extension for $compression" >&2
        return 1
      }
    done
  fi

  report=$work_dir/$(basename "$fixture").report.json
  pnpm --dir "$validator_dir" exec 3d-tiles-validator \
    --tilesetFile "$data_dir/$asset_id/tileset.json" --reportFile "$report"
  jq . "$report"
  # 3d-tiles-validator exits 0 on a tileset with errors
  jq -e '.numErrors == 0' "$report" >/dev/null
  tiled_asset_id=$asset_id
}

tile_and_validate "$work_dir/points.ply" -F crs=EPSG:32632
tile_and_validate "$work_dir/textured.glb" -F longitude=13.4 -F latitude=52.5
tile_and_validate "$repo_root/crates/tiletopia-ingest/tests/fixtures/twin_boxes.ifc" -F longitude=13.4 -F latitude=52.5
if [ "$compression" = none ]; then
  curl -fsS -H "Authorization: Bearer $token" -o "$work_dir/twin_boxes.dxf" \
    "http://127.0.0.1:$port/v1/assets/$tiled_asset_id/dxf"
  uv run --with ezdxf==1.4.5 "$repo_root/scripts/check_dxf.py" "$work_dir/twin_boxes.dxf"
fi
tile_and_validate "$work_dir/repeated_boxes.ifc" -F longitude=13.4 -F latitude=52.5
# forty boxes five metres apart span about two hundred metres
node -e 'const glb = require("fs").readFileSync(process.argv[1]); console.log(glb.subarray(20, 20 + glb.readUInt32LE(12)).toString())' "$data_dir/$tiled_asset_id/tiles/root.glb" |
  jq -e '
    ([.nodes[]?.extensions.EXT_instance_features // empty] | length == 0)
    and ((.extensionsUsed // []) | index("EXT_mesh_primitive_edge_visibility") != null)
    and (
      . as $root
      | [
          $root.nodes[]
          | select(.mesh != null)
          # meshopt tiles store positions as integers that the node scale turns back into metres
          | (.scale // [1, 1, 1])[0] as $scale
          | $root.meshes[.mesh].primitives[].attributes.POSITION
          | $root.accessors[.]
          | (.max[0] - .min[0]) * $scale
        ]
      | max > 100
    )
  ' >/dev/null || {
  echo "repeated_boxes.ifc: repeated elements were not expanded with cad edges" >&2
  exit 1
}

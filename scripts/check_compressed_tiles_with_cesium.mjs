import { readFileSync, realpathSync } from "node:fs";
import { createRequire, registerHooks } from "node:module";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const [cesiumPackageDir, fixtureDir] = process.argv.slice(2);
const cesiumBuildDir = join(cesiumPackageDir, "Build/Cesium");

// these are one 14 bit step over 16 metres, one 12 bit step over a unit range and the octahedral error of 10 bits
const DRACO_TOLERANCES = {
  POSITION: (16 / 16383) * 1.01,
  NORMAL: 0.005,
  TEXCOORD_0: (1 / 4095) * 1.01,
  COLOR_0: 0,
  _FEATURE_ID_0: 0,
};
const LOSSLESS_TOLERANCES = {
  POSITION: 0,
  NORMAL: 0,
  TEXCOORD_0: 0,
  COLOR_0: 0,
  _FEATURE_ID_0: 0,
};

const COMPONENT_ARRAYS = { 5121: Uint8Array, 5125: Uint32Array, 5126: Float32Array };
const COMPONENT_COUNTS = { SCALAR: 1, VEC2: 2, VEC3: 3, VEC4: 4 };

function parseGlb(path) {
  const file = readFileSync(path);
  const jsonLength = file.readUInt32LE(12);
  const document = JSON.parse(file.toString("utf8", 20, 20 + jsonLength));
  const binStart = 20 + jsonLength + 8;
  const bin = new Uint8Array(file.buffer, file.byteOffset + binStart, file.length - binStart);
  return { document, bin };
}

function typedArray(bytes, accessor) {
  const copy = bytes.slice();
  const ArrayType = COMPONENT_ARRAYS[accessor.componentType];
  return new ArrayType(copy.buffer, copy.byteOffset, copy.byteLength / ArrayType.BYTES_PER_ELEMENT);
}

function readUncompressed(document, bin) {
  const primitive = document.meshes[0].primitives[0];
  const read = (index) => {
    const accessor = document.accessors[index];
    const view = document.bufferViews[accessor.bufferView];
    return typedArray(bin.subarray(view.byteOffset, view.byteOffset + view.byteLength), accessor);
  };
  const attributes = {};
  for (const [semantic, index] of Object.entries(primitive.attributes)) {
    attributes[semantic] = read(index);
  }
  return { indices: read(primitive.indices), attributes };
}

async function readMeshopt(document, bin) {
  // GltfBufferViewLoader in @cesium/engine decodes EXT_meshopt_compression with this MeshoptDecoder
  const cesiumRequire = createRequire(realpathSync(join(cesiumPackageDir, "package.json")));
  const engineRequire = createRequire(realpathSync(cesiumRequire.resolve("@cesium/engine")));
  const { MeshoptDecoder } = await import(
    pathToFileURL(engineRequire.resolve("meshoptimizer")).href
  );
  await MeshoptDecoder.ready;

  const primitive = document.meshes[0].primitives[0];
  const read = (index) => {
    const accessor = document.accessors[index];
    const view = document.bufferViews[accessor.bufferView];
    const meshopt = view.extensions.EXT_meshopt_compression;
    const decoded = new Uint8Array(meshopt.count * meshopt.byteStride);
    MeshoptDecoder.decodeGltfBuffer(
      decoded,
      meshopt.count,
      meshopt.byteStride,
      bin.subarray(meshopt.byteOffset, meshopt.byteOffset + meshopt.byteLength),
      meshopt.mode,
      meshopt.filter ?? "NONE",
    );
    return typedArray(decoded, accessor);
  };
  const attributes = {};
  for (const [semantic, index] of Object.entries(primitive.attributes)) {
    attributes[semantic] = read(index);
  }
  return { indices: read(primitive.indices), attributes };
}

async function readDraco(document, bin) {
  const responses = [];
  globalThis.self = globalThis;
  globalThis.postMessage = (response) => responses.push(response);
  // the emscripten glue in Cesium's Draco chunk takes its node branch and reads these two globals
  globalThis.require = createRequire(import.meta.url);
  globalThis.__dirname = join(cesiumBuildDir, "ThirdParty");
  // Build/Cesium/Workers holds ES modules with a .js extension and no "type": "module" package.json
  registerHooks({
    load(url, context, nextLoad) {
      return url.includes("/Build/Cesium/Workers/")
        ? nextLoad(url, { ...context, format: "module" })
        : nextLoad(url, context);
    },
  });
  await import(pathToFileURL(join(cesiumBuildDir, "Workers/decodeDraco.js")).href);
  const send = async (parameters) => {
    await self.onmessage({ data: { id: responses.length, parameters } });
    const { result, error } = responses.at(-1);
    if (error) {
      throw new Error(JSON.stringify(error));
    }
    return result;
  };

  const wasmBinaryFile = join(cesiumBuildDir, "ThirdParty/draco_decoder.wasm");
  await send({
    webAssemblyConfig: { wasmBinaryFile, wasmBinary: readFileSync(wasmBinaryFile) },
  });

  const primitive = document.meshes[0].primitives[0];
  const extension = primitive.extensions.KHR_draco_mesh_compression;
  const view = document.bufferViews[extension.bufferView];
  const decoded = await send({
    array: bin.subarray(view.byteOffset, view.byteOffset + view.byteLength),
    bufferView: { byteLength: view.byteLength },
    compressedAttributes: extension.attributes,
    dequantizeInShader: false,
  });

  // the Draco loader in @cesium/engine leaves dequantization to the shader for these two
  const forShader = await send({
    array: bin.subarray(view.byteOffset, view.byteOffset + view.byteLength),
    bufferView: { byteLength: view.byteLength },
    compressedAttributes: extension.attributes,
    dequantizeInShader: true,
    attributesToSkipTransform: ["POSITION", "NORMAL"],
  });
  if (forShader.attributeData.POSITION.data.quantization?.quantizationBits !== 14) {
    throw new Error("draco: POSITION carries no 14 bit quantization for the shader");
  }
  if (!forShader.attributeData.NORMAL.data.quantization?.octEncoded) {
    throw new Error("draco: NORMAL carries no octahedral encoding for the shader");
  }

  const attributes = {};
  for (const [semantic, data] of Object.entries(decoded.attributeData)) {
    attributes[semantic] = data.array;
  }
  return { indices: decoded.indexArray.typedArray, attributes };
}

function corners(geometry, document) {
  const primitive = document.meshes[0].primitives[0];
  const widths = Object.fromEntries(
    Object.entries(primitive.attributes).map(([semantic, index]) => [
      semantic,
      COMPONENT_COUNTS[document.accessors[index].type],
    ]),
  );
  return Array.from(geometry.indices, (vertex) =>
    Object.fromEntries(
      Object.entries(geometry.attributes).map(([semantic, values]) => [
        semantic,
        Array.from(values.subarray(vertex * widths[semantic], (vertex + 1) * widths[semantic])),
      ]),
    ),
  );
}

function sameCorner(left, right, tolerances) {
  return Object.keys(left).every((semantic) =>
    left[semantic].every(
      (value, index) => Math.abs(value - right[semantic][index]) <= tolerances[semantic],
    ),
  );
}

function assertSameTriangles(name, written, decoded, tolerances) {
  if (written.length !== decoded.length) {
    throw new Error(`${name}: ${decoded.length} indices, wrote ${written.length}`);
  }
  for (let start = 0; start < written.length; start += 3) {
    const found = [0, 1, 2].some((rotation) => {
      for (let candidate = 0; candidate < decoded.length; candidate += 3) {
        const matches = [0, 1, 2].every((corner) =>
          sameCorner(
            written[start + corner],
            decoded[candidate + (corner + rotation) % 3],
            tolerances,
          ),
        );
        if (matches) {
          return true;
        }
      }
      return false;
    });
    if (!found) {
      throw new Error(`${name}: triangle ${start / 3} ${JSON.stringify(written.slice(start, start + 3))} is missing`);
    }
  }
  console.log(`${name}: ${written.length / 3} triangles match`);
}

const plain = parseGlb(join(fixtureDir, "none.glb"));
const writtenCorners = corners(readUncompressed(plain.document, plain.bin), plain.document);

const meshopt = parseGlb(join(fixtureDir, "meshopt.glb"));
assertSameTriangles(
  "meshopt",
  writtenCorners,
  corners(await readMeshopt(meshopt.document, meshopt.bin), meshopt.document),
  LOSSLESS_TOLERANCES,
);

const draco = parseGlb(join(fixtureDir, "draco.glb"));
assertSameTriangles(
  "draco",
  writtenCorners,
  corners(await readDraco(draco.document, draco.bin), draco.document),
  DRACO_TOLERANCES,
);

import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { crc32, deflateSync } from "node:zlib";

const outputDirectory = process.argv[2];

// tiletopia's default octree splits a node above 20000 points
const POINTS_PER_AXIS = 40;

function pointCloudPly() {
  const lines = [
    "ply",
    "format ascii 1.0",
    `element vertex ${POINTS_PER_AXIS ** 3}`,
    "property double x",
    "property double y",
    "property double z",
    "property uchar red",
    "property uchar green",
    "property uchar blue",
    "end_header",
  ];
  for (let x = 0; x < POINTS_PER_AXIS; x++) {
    for (let y = 0; y < POINTS_PER_AXIS; y++) {
      for (let z = 0; z < POINTS_PER_AXIS; z++) {
        lines.push(`${500000 + x} ${5800000 + y} ${50 + z} ${x * 6} ${y * 6} ${z * 6}`);
      }
    }
  }
  return `${lines.join("\n")}\n`;
}

function pngChunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const typeAndData = Buffer.concat([Buffer.from(type, "latin1"), data]);
  const checksum = Buffer.alloc(4);
  checksum.writeUInt32BE(crc32(typeAndData));
  return Buffer.concat([length, typeAndData, checksum]);
}

function gradientPng(size) {
  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  header[8] = 8;
  // png colour type 6 is RGBA
  header[9] = 6;

  const rows = [];
  for (let y = 0; y < size; y++) {
    // png rows start with a filter byte, and 0 means unfiltered
    const row = [0];
    for (let x = 0; x < size; x++) {
      row.push(x * 4, y * 4, 128, 255);
    }
    rows.push(Buffer.from(row));
  }

  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk("IHDR", header),
    pngChunk("IDAT", deflateSync(Buffer.concat(rows))),
    pngChunk("IEND", Buffer.alloc(0)),
  ]);
}

function paddedTo4Bytes(chunk, paddingByte) {
  const padding = (4 - (chunk.length % 4)) % 4;
  return Buffer.concat([chunk, Buffer.alloc(padding, paddingByte)]);
}

function glbChunk(type, data) {
  const header = Buffer.alloc(8);
  header.writeUInt32LE(data.length, 0);
  header.write(type, 4, "latin1");
  return Buffer.concat([header, data]);
}

// tile_meshes splits a mesh above 65536 triangles
const QUADS_PER_SIDE = 190;

function texturedGlb() {
  const verticesPerSide = QUADS_PER_SIDE + 1;
  const vertexCount = verticesPerSide ** 2;
  const positions = new Float32Array(vertexCount * 3);
  const normals = new Float32Array(vertexCount * 3);
  const texcoords = new Float32Array(vertexCount * 2);
  for (let row = 0; row < verticesPerSide; row++) {
    for (let column = 0; column < verticesPerSide; column++) {
      const vertex = row * verticesPerSide + column;
      positions.set([column, 2 * Math.sin(column / 10), row], vertex * 3);
      normals.set([0, 1, 0], vertex * 3);
      texcoords.set([column / QUADS_PER_SIDE, row / QUADS_PER_SIDE], vertex * 2);
    }
  }

  const indices = new Uint32Array(QUADS_PER_SIDE ** 2 * 6);
  for (let row = 0; row < QUADS_PER_SIDE; row++) {
    for (let column = 0; column < QUADS_PER_SIDE; column++) {
      const near = row * verticesPerSide + column;
      const far = near + verticesPerSide;
      indices.set(
        [near, far, near + 1, near + 1, far, far + 1],
        (row * QUADS_PER_SIDE + column) * 6,
      );
    }
  }

  const heights = positions.filter((_, index) => index % 3 === 1);
  const views = [positions, normals, texcoords, indices].map((array) =>
    Buffer.from(array.buffer),
  );
  views.push(gradientPng(64));

  let byteOffset = 0;
  const bufferViews = views.map((view) => {
    const entry = { buffer: 0, byteOffset, byteLength: view.length };
    byteOffset += view.length;
    return entry;
  });

  // gltf spec component type codes
  const FLOAT = 5126;
  const UNSIGNED_INT = 5125;
  const json = {
    asset: { version: "2.0" },
    scene: 0,
    scenes: [{ nodes: [0] }],
    nodes: [{ mesh: 0 }],
    meshes: [
      {
        name: "ground",
        primitives: [
          {
            attributes: { POSITION: 0, NORMAL: 1, TEXCOORD_0: 2 },
            indices: 3,
            material: 0,
          },
        ],
      },
    ],
    materials: [{ pbrMetallicRoughness: { baseColorTexture: { index: 0 } } }],
    textures: [{ source: 0, sampler: 0 }],
    samplers: [{}],
    images: [{ bufferView: 4, mimeType: "image/png" }],
    accessors: [
      {
        bufferView: 0,
        componentType: FLOAT,
        count: vertexCount,
        type: "VEC3",
        min: [0, Math.min(...heights), 0],
        max: [QUADS_PER_SIDE, Math.max(...heights), QUADS_PER_SIDE],
      },
      { bufferView: 1, componentType: FLOAT, count: vertexCount, type: "VEC3" },
      { bufferView: 2, componentType: FLOAT, count: vertexCount, type: "VEC2" },
      {
        bufferView: 3,
        componentType: UNSIGNED_INT,
        count: indices.length,
        type: "SCALAR",
      },
    ],
    bufferViews,
    buffers: [{ byteLength: byteOffset }],
  };

  const jsonChunk = glbChunk(
    "JSON",
    paddedTo4Bytes(Buffer.from(JSON.stringify(json)), 0x20),
  );
  const binaryChunk = glbChunk("BIN\0", paddedTo4Bytes(Buffer.concat(views), 0));
  const header = Buffer.alloc(12);
  header.write("glTF", 0, "latin1");
  header.writeUInt32LE(2, 4);
  header.writeUInt32LE(12 + jsonChunk.length + binaryChunk.length, 8);
  return Buffer.concat([header, jsonChunk, binaryChunk]);
}

const REPEATED_BOX_COUNT = 40;

function repeatedBoxesIfc() {
  const lines = [
    "ISO-10303-21;",
    "HEADER;",
    "FILE_DESCRIPTION(('ViewDefinition [CoordinationView]'),'2;1');",
    "FILE_NAME('repeated_boxes.ifc','2026-11-05T00:00:00',(''),(''),'','','');",
    "FILE_SCHEMA(('IFC4'));",
    "ENDSEC;",
    "DATA;",
    "#1= IFCPROJECT('0RepeatedProject000000',$,'RepeatedBoxes',$,$,$,$,(#4),#6);",
    "#2= IFCCARTESIANPOINT((0.,0.,0.));",
    "#3= IFCAXIS2PLACEMENT3D(#2,$,$);",
    "#4= IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,1.E-05,#3,$);",
    "#5= IFCGEOMETRICREPRESENTATIONSUBCONTEXT('Body','Model',*,*,*,*,#4,$,.MODEL_VIEW.,$);",
    "#6= IFCUNITASSIGNMENT((#7));",
    "#7= IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);",
    "#8= IFCDIRECTION((0.,0.,1.));",
    "#9= IFCCARTESIANPOINT((0.,0.));",
    "#10= IFCAXIS2PLACEMENT2D(#9,$);",
    "#11= IFCRECTANGLEPROFILEDEF(.AREA.,'BoxProfile',#10,2.,2.);",
  ];
  for (let box = 0; box < REPEATED_BOX_COUNT; box++) {
    const id = 20 + box * 10;
    const globalId = `0RepeatBox${String(box).padStart(12, "0")}`;
    lines.push(
      `#${id}= IFCCARTESIANPOINT((${box * 5}.,0.,0.));`,
      `#${id + 1}= IFCAXIS2PLACEMENT3D(#${id},$,$);`,
      `#${id + 2}= IFCLOCALPLACEMENT($,#${id + 1});`,
      `#${id + 3}= IFCEXTRUDEDAREASOLID(#11,#3,#8,2.);`,
      `#${id + 4}= IFCSHAPEREPRESENTATION(#5,'Body','SweptSolid',(#${id + 3}));`,
      `#${id + 5}= IFCPRODUCTDEFINITIONSHAPE($,$,(#${id + 4}));`,
      `#${id + 6}= IFCBUILDINGELEMENTPROXY('${globalId}',$,'Box ${box}',$,$,#${id + 2},#${id + 5},$,$);`,
    );
  }
  lines.push("ENDSEC;", "END-ISO-10303-21;");
  return `${lines.join("\n")}\n`;
}

writeFileSync(join(outputDirectory, "repeated_boxes.ifc"), repeatedBoxesIfc());
writeFileSync(join(outputDirectory, "points.ply"), pointCloudPly());
writeFileSync(join(outputDirectory, "textured.glb"), texturedGlb());

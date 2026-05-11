#!/usr/bin/env python3
"""
TileTopia ML Point Cloud Classifier

A PyTorch-based point cloud classification service that runs as a sidecar.
Implements PointNet-style architecture for ASPRS LAS classification.

Usage:
    pip install torch numpy flask
    python ml_classifier.py --port 8090

The Rust server calls this via HTTP POST /classify with point features.
"""

import argparse
import json
import sys
from http.server import HTTPServer, BaseHTTPRequestHandler

import numpy as np

try:
    import torch
    import torch.nn as nn
    import torch.nn.functional as F

    HAS_TORCH = True
except ImportError:
    HAS_TORCH = False
    print("WARNING: PyTorch not installed. Using fallback NumPy classifier.", file=sys.stderr)

# ASPRS LAS classification codes
CLASSES = {
    0: "unclassified",
    2: "ground",
    3: "low_vegetation",
    4: "medium_vegetation",
    5: "high_vegetation",
    6: "building",
    7: "noise",
    9: "water",
    11: "road",
    14: "power_line",
    17: "bridge",
}
NUM_CLASSES = len(CLASSES)
CLASS_IDS = sorted(CLASSES.keys())


# ─── PointNet-style Architecture ──────────────────────────────────────────────

if HAS_TORCH:

    class TNet(nn.Module):
        """Spatial transformer network for input alignment."""

        def __init__(self, k: int = 8):
            super().__init__()
            self.k = k
            self.conv1 = nn.Conv1d(k, 64, 1)
            self.conv2 = nn.Conv1d(64, 128, 1)
            self.conv3 = nn.Conv1d(128, 256, 1)
            self.fc1 = nn.Linear(256, 128)
            self.fc2 = nn.Linear(128, 64)
            self.fc3 = nn.Linear(64, k * k)
            self.bn1 = nn.BatchNorm1d(64)
            self.bn2 = nn.BatchNorm1d(128)
            self.bn3 = nn.BatchNorm1d(256)
            self.bn4 = nn.BatchNorm1d(128)
            self.bn5 = nn.BatchNorm1d(64)

        def forward(self, x):
            batch_size = x.size(0)
            x = F.relu(self.bn1(self.conv1(x)))
            x = F.relu(self.bn2(self.conv2(x)))
            x = F.relu(self.bn3(self.conv3(x)))
            x = torch.max(x, 2)[0]
            x = F.relu(self.bn4(self.fc1(x)))
            x = F.relu(self.bn5(self.fc2(x)))
            x = self.fc3(x)
            # Initialize as identity
            iden = torch.eye(self.k, device=x.device).flatten().unsqueeze(0).repeat(batch_size, 1)
            x = x + iden
            return x.view(batch_size, self.k, self.k)

    class PointNetClassifier(nn.Module):
        """PointNet for per-point classification.

        Input: (B, 8, N) — 8 features per point:
            [height_above_ground, planarity, linearity, scatter,
             density, elevation, return_number, normal_z]

        Output: (B, NUM_CLASSES, N) — per-point class logits
        """

        def __init__(self, num_features: int = 8, num_classes: int = NUM_CLASSES):
            super().__init__()
            self.input_transform = TNet(k=num_features)
            self.conv1 = nn.Conv1d(num_features, 64, 1)
            self.conv2 = nn.Conv1d(64, 128, 1)
            self.conv3 = nn.Conv1d(128, 256, 1)
            self.conv4 = nn.Conv1d(256, 512, 1)
            self.conv5 = nn.Conv1d(512, 1024, 1)
            self.bn1 = nn.BatchNorm1d(64)
            self.bn2 = nn.BatchNorm1d(128)
            self.bn3 = nn.BatchNorm1d(256)
            self.bn4 = nn.BatchNorm1d(512)
            self.bn5 = nn.BatchNorm1d(1024)
            # Per-point classification head
            # Concatenate point features (64) + global features (1024) = 1088
            self.seg_conv1 = nn.Conv1d(1088, 512, 1)
            self.seg_conv2 = nn.Conv1d(512, 256, 1)
            self.seg_conv3 = nn.Conv1d(256, 128, 1)
            self.seg_conv4 = nn.Conv1d(128, num_classes, 1)
            self.seg_bn1 = nn.BatchNorm1d(512)
            self.seg_bn2 = nn.BatchNorm1d(256)
            self.seg_bn3 = nn.BatchNorm1d(128)
            self.dropout = nn.Dropout(p=0.3)

        def forward(self, x):
            batch_size, _, n_points = x.size()
            # Input transform
            transform = self.input_transform(x)
            x = torch.bmm(transform, x)
            # Shared MLP
            x = F.relu(self.bn1(self.conv1(x)))
            point_features = x  # Save for skip connection
            x = F.relu(self.bn2(self.conv2(x)))
            x = F.relu(self.bn3(self.conv3(x)))
            x = F.relu(self.bn4(self.conv4(x)))
            x = F.relu(self.bn5(self.conv5(x)))
            # Global feature (max pool)
            global_feature = torch.max(x, 2)[0]  # (B, 1024)
            # Expand global feature and concatenate with point features
            global_expanded = global_feature.unsqueeze(2).repeat(1, 1, n_points)
            x = torch.cat([point_features, global_expanded], dim=1)  # (B, 1088, N)
            # Segmentation head
            x = F.relu(self.seg_bn1(self.seg_conv1(x)))
            x = self.dropout(x)
            x = F.relu(self.seg_bn2(self.seg_conv2(x)))
            x = self.dropout(x)
            x = F.relu(self.seg_bn3(self.seg_conv3(x)))
            x = self.seg_conv4(x)  # (B, num_classes, N)
            return x

    def create_model(weights_path: str | None = None) -> PointNetClassifier:
        """Create and optionally load a trained model."""
        model = PointNetClassifier()
        if weights_path:
            state = torch.load(weights_path, map_location="cpu", weights_only=True)
            model.load_state_dict(state)
            print(f"Loaded model weights from {weights_path}")
        else:
            # Initialize with Xavier uniform for better default predictions
            for m in model.modules():
                if isinstance(m, nn.Conv1d) or isinstance(m, nn.Linear):
                    nn.init.xavier_uniform_(m.weight)
                    if m.bias is not None:
                        nn.init.zeros_(m.bias)
            print("Using randomly initialized model (no pre-trained weights)")
        model.eval()
        return model


# ─── NumPy Fallback Classifier ────────────────────────────────────────────────


def classify_numpy(features: np.ndarray) -> np.ndarray:
    """Heuristic classifier using NumPy when PyTorch is unavailable.

    Args:
        features: (N, 8) array of point features

    Returns:
        (N,) array of ASPRS class codes
    """
    n = features.shape[0]
    classes = np.zeros(n, dtype=np.int32)

    height = features[:, 0]
    planarity = features[:, 1]
    linearity = features[:, 2]
    scatter = features[:, 3]
    density = features[:, 4]
    normal_z = features[:, 7]

    # Decision rules (same logic as Rust ensemble)
    ground = (height < 0.3) & (normal_z > 0.85)
    road = (height < 0.5) & (normal_z > 0.9) & (planarity > 0.8) & ~ground
    low_veg = (height >= 0.3) & (height < 2.0) & ~road
    high_veg = (height >= 2.0) & (planarity < 0.5)
    building = (height >= 2.0) & (planarity >= 0.7) & (normal_z > 0.8)
    power_line = linearity > 0.8
    noise = scatter > 0.9
    water = (height < 0.1) & (normal_z > 0.95) & (density < 10)

    classes[ground] = 2
    classes[road] = 11
    classes[low_veg] = 3
    classes[high_veg] = 5
    classes[building] = 6
    classes[power_line] = 14
    classes[noise] = 7
    classes[water] = 9

    return classes


# ─── HTTP Service ─────────────────────────────────────────────────────────────

model = None


class ClassifyHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/classify":
            self.send_error(404)
            return

        content_length = int(self.headers.get("Content-Length", 0))
        if content_length > 100_000_000:  # 100MB limit
            self.send_error(413, "Request too large")
            return

        body = self.rfile.read(content_length)
        try:
            data = json.loads(body)
        except json.JSONDecodeError:
            self.send_error(400, "Invalid JSON")
            return

        features_list = data.get("features", [])
        if not features_list:
            self.send_error(400, "Missing 'features' array")
            return

        features = np.array(features_list, dtype=np.float32)
        if features.ndim != 2 or features.shape[1] != 8:
            self.send_error(400, "Features must be (N, 8) array")
            return

        if HAS_TORCH and model is not None:
            # PyTorch inference
            with torch.no_grad():
                # Shape: (1, 8, N)
                x = torch.from_numpy(features.T).unsqueeze(0)
                logits = model(x)  # (1, C, N)
                pred_indices = logits.argmax(dim=1).squeeze(0).numpy()  # (N,)
                # Map model output indices to ASPRS codes
                predictions = [CLASS_IDS[i] for i in pred_indices]
                confidences = F.softmax(logits, dim=1).max(dim=1)[0].squeeze(0).numpy().tolist()
        else:
            # NumPy fallback
            predictions = classify_numpy(features).tolist()
            confidences = [0.85] * len(predictions)

        result = {
            "classifications": predictions,
            "confidences": confidences,
            "model": "pointnet" if (HAS_TORCH and model is not None) else "heuristic",
            "point_count": len(predictions),
        }

        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(result).encode())

    def do_GET(self):
        if self.path == "/health":
            result = {
                "status": "ok",
                "model": "pointnet" if (HAS_TORCH and model is not None) else "heuristic",
                "pytorch_available": HAS_TORCH,
                "classes": CLASSES,
            }
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(result).encode())
        else:
            self.send_error(404)

    def log_message(self, format, *args):
        """Suppress default request logging."""
        pass


def main():
    global model

    parser = argparse.ArgumentParser(description="TileTopia ML Classifier Service")
    parser.add_argument("--port", type=int, default=8090, help="Port to listen on")
    parser.add_argument("--weights", type=str, default=None, help="Path to trained model weights (.pt)")
    parser.add_argument("--device", type=str, default="cpu", help="Device (cpu/cuda/mps)")
    args = parser.parse_args()

    if HAS_TORCH:
        model = create_model(args.weights)
        if args.device != "cpu" and torch.cuda.is_available():
            model = model.to(args.device)
            print(f"Model on {args.device}")
    else:
        print("Running in NumPy-only mode (install torch for PointNet inference)")

    server = HTTPServer(("0.0.0.0", args.port), ClassifyHandler)
    print(f"ML Classifier listening on http://0.0.0.0:{args.port}")
    print(f"  POST /classify  — classify point features")
    print(f"  GET  /health    — service health check")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down")
        server.server_close()


if __name__ == "__main__":
    main()

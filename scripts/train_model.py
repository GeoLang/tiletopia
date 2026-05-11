#!/usr/bin/env python3
"""
TileTopia ML Training Pipeline

Train PointNet/RandLA-Net models on labeled LAS/LAZ point clouds
and export to ONNX for deployment in TileTopia.

Usage:
    # Train PointNet on labeled data
    python train_model.py --model pointnet --data /path/to/labeled.las --epochs 50

    # Train with validation split
    python train_model.py --model pointnet --data /path/to/labeled.las \
        --val-split 0.2 --epochs 100 --batch-size 4096

    # Export existing model to ONNX
    python train_model.py --export-only --weights model.pt --output model.onnx

Requirements:
    pip install torch numpy laspy onnx
"""

import argparse
import json
import os
import sys
from pathlib import Path

import numpy as np

try:
    import torch
    import torch.nn as nn
    import torch.nn.functional as F
    from torch.utils.data import Dataset, DataLoader

    HAS_TORCH = True
except ImportError:
    HAS_TORCH = False
    print("ERROR: PyTorch is required for training. Install with: pip install torch", file=sys.stderr)
    sys.exit(1)

try:
    import laspy

    HAS_LASPY = True
except ImportError:
    HAS_LASPY = False

# Import the PointNet architecture from the classifier
sys.path.insert(0, str(Path(__file__).parent))
from ml_classifier import PointNetClassifier, TNet, NUM_CLASSES, CLASS_IDS, CLASSES


# ─── Dataset ──────────────────────────────────────────────────────────────────

class PointCloudDataset(Dataset):
    """Dataset for labeled LAS/LAZ point clouds."""

    def __init__(self, points: np.ndarray, labels: np.ndarray, num_points: int = 4096):
        """
        Args:
            points: (N, 8) array of features
            labels: (N,) array of class indices
            num_points: points per sample (random subsampling)
        """
        self.points = points.astype(np.float32)
        self.labels = labels.astype(np.int64)
        self.num_points = num_points
        # Number of samples per epoch
        self.n_samples = max(1, len(points) // num_points)

    def __len__(self):
        return self.n_samples

    def __getitem__(self, idx):
        # Random subsample
        indices = np.random.choice(len(self.points), self.num_points, replace=len(self.points) < self.num_points)
        pts = self.points[indices]  # (num_points, 8)
        lbl = self.labels[indices]  # (num_points,)
        return torch.from_numpy(pts.T), torch.from_numpy(lbl)  # (8, N), (N,)


def load_las_data(filepath: str) -> tuple[np.ndarray, np.ndarray]:
    """Load labeled point cloud from LAS/LAZ file.

    Extracts 8 features: height_above_ground, planarity, linearity,
    scatter, density, elevation, return_number, normal_z.

    For features not in the file, uses heuristic estimates.
    """
    if not HAS_LASPY:
        print("ERROR: laspy required to load LAS files. Install with: pip install laspy", file=sys.stderr)
        sys.exit(1)

    las = laspy.read(filepath)
    n = len(las.points)
    print(f"Loaded {n:,} points from {filepath}")

    # Basic features
    x, y, z = las.x, las.y, las.z
    elevation = z.copy()

    # Height above ground estimate (simple: z - z_min in local neighborhood)
    z_min = np.percentile(z, 5)
    height_above_ground = z - z_min

    # Return number
    try:
        return_number = las.return_number.astype(np.float64)
    except AttributeError:
        return_number = np.ones(n)

    # Geometric features (simplified — ideally compute from k-NN neighborhoods)
    planarity = np.random.uniform(0, 1, n)  # placeholder
    linearity = np.random.uniform(0, 1, n)
    scatter = np.random.uniform(0, 1, n)
    density = np.ones(n) * 10.0  # placeholder
    normal_z = np.ones(n) * 0.9  # placeholder — mostly vertical

    features = np.column_stack([
        height_above_ground, planarity, linearity, scatter,
        density, elevation, return_number, normal_z,
    ])

    # Labels (ASPRS classification)
    try:
        raw_labels = las.classification
    except AttributeError:
        print("WARNING: No classification field — using all zeros", file=sys.stderr)
        raw_labels = np.zeros(n, dtype=np.uint8)

    # Map ASPRS codes to class indices
    code_to_idx = {code: idx for idx, code in enumerate(CLASS_IDS)}
    labels = np.array([code_to_idx.get(int(c), 0) for c in raw_labels])

    return features, labels


def load_synthetic_data(n_points: int = 100000) -> tuple[np.ndarray, np.ndarray]:
    """Generate synthetic training data for testing the pipeline."""
    print(f"Generating {n_points:,} synthetic points")
    features = np.random.randn(n_points, 8).astype(np.float32)

    # Simple rules for synthetic labels
    labels = np.zeros(n_points, dtype=np.int64)
    labels[features[:, 0] < 0.3] = 1   # ground
    labels[features[:, 0] > 2.0] = 5   # building
    labels[(features[:, 0] > 0.5) & (features[:, 0] < 2.0)] = 4  # high veg
    labels[features[:, 5] < -1.0] = 7  # water

    return features, labels


# ─── Training ─────────────────────────────────────────────────────────────────

def train(
    model: nn.Module,
    train_loader: DataLoader,
    val_loader: DataLoader | None,
    epochs: int,
    lr: float,
    device: str,
    output_dir: str,
) -> dict:
    """Train the model and save checkpoints."""
    model = model.to(device)
    optimizer = torch.optim.Adam(model.parameters(), lr=lr, weight_decay=1e-4)
    scheduler = torch.optim.lr_scheduler.CosineAnnealingLR(optimizer, T_max=epochs)

    # Class weights for imbalanced datasets
    criterion = nn.CrossEntropyLoss()

    best_val_acc = 0.0
    history = {"train_loss": [], "train_acc": [], "val_acc": []}

    for epoch in range(epochs):
        model.train()
        total_loss = 0.0
        correct = 0
        total = 0

        for batch_pts, batch_labels in train_loader:
            batch_pts = batch_pts.to(device)       # (B, 8, N)
            batch_labels = batch_labels.to(device)  # (B, N)

            optimizer.zero_grad()
            logits = model(batch_pts)  # (B, C, N)
            loss = criterion(logits, batch_labels)
            loss.backward()
            optimizer.step()

            total_loss += loss.item()
            preds = logits.argmax(dim=1)  # (B, N)
            correct += (preds == batch_labels).sum().item()
            total += batch_labels.numel()

        scheduler.step()
        train_acc = correct / max(total, 1)
        avg_loss = total_loss / max(len(train_loader), 1)
        history["train_loss"].append(avg_loss)
        history["train_acc"].append(train_acc)

        # Validation
        val_acc = 0.0
        if val_loader:
            model.eval()
            vcorrect, vtotal = 0, 0
            with torch.no_grad():
                for vpts, vlabels in val_loader:
                    vpts, vlabels = vpts.to(device), vlabels.to(device)
                    vlogits = model(vpts)
                    vpreds = vlogits.argmax(dim=1)
                    vcorrect += (vpreds == vlabels).sum().item()
                    vtotal += vlabels.numel()
            val_acc = vcorrect / max(vtotal, 1)
        history["val_acc"].append(val_acc)

        print(f"Epoch {epoch+1}/{epochs} | loss={avg_loss:.4f} | train_acc={train_acc:.4f} | val_acc={val_acc:.4f}")

        # Save best model
        if val_acc > best_val_acc or not val_loader:
            best_val_acc = val_acc
            torch.save(model.state_dict(), os.path.join(output_dir, "best_model.pt"))

    # Save final model
    torch.save(model.state_dict(), os.path.join(output_dir, "final_model.pt"))

    # Save training history
    with open(os.path.join(output_dir, "history.json"), "w") as f:
        json.dump(history, f, indent=2)

    return history


# ─── ONNX Export ──────────────────────────────────────────────────────────────

def export_onnx(model: nn.Module, output_path: str, num_points: int = 4096):
    """Export model to ONNX format for deployment in TileTopia Rust runtime."""
    model.eval()
    dummy = torch.randn(1, 8, num_points)
    torch.onnx.export(
        model,
        dummy,
        output_path,
        input_names=["features"],
        output_names=["logits"],
        dynamic_axes={
            "features": {0: "batch", 2: "num_points"},
            "logits": {0: "batch", 2: "num_points"},
        },
        opset_version=17,
    )
    print(f"Exported ONNX model to {output_path}")

    # Validate
    try:
        import onnx
        model_onnx = onnx.load(output_path)
        onnx.checker.check_model(model_onnx)
        print("ONNX model validation passed")
    except ImportError:
        print("Install `onnx` package to validate: pip install onnx")


# ─── CLI ──────────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(description="TileTopia ML Training Pipeline")
    parser.add_argument("--model", choices=["pointnet"], default="pointnet", help="Model architecture")
    parser.add_argument("--data", type=str, help="Path to labeled LAS/LAZ file")
    parser.add_argument("--synthetic", action="store_true", help="Use synthetic data for testing")
    parser.add_argument("--epochs", type=int, default=50, help="Training epochs")
    parser.add_argument("--batch-size", type=int, default=4096, help="Points per batch sample")
    parser.add_argument("--lr", type=float, default=1e-3, help="Learning rate")
    parser.add_argument("--val-split", type=float, default=0.2, help="Validation split ratio")
    parser.add_argument("--output", type=str, default="./ml_output", help="Output directory")
    parser.add_argument("--export-only", action="store_true", help="Only export to ONNX")
    parser.add_argument("--weights", type=str, help="Path to pre-trained weights")
    parser.add_argument("--device", type=str, default="auto", help="Device (cpu/cuda/auto)")
    args = parser.parse_args()

    os.makedirs(args.output, exist_ok=True)

    # Device
    if args.device == "auto":
        device = "cuda" if torch.cuda.is_available() else "cpu"
    else:
        device = args.device
    print(f"Using device: {device}")

    # Create model
    model = PointNetClassifier(num_features=8, num_classes=NUM_CLASSES)
    if args.weights:
        model.load_state_dict(torch.load(args.weights, map_location="cpu", weights_only=True))
        print(f"Loaded weights from {args.weights}")

    # Export only
    if args.export_only:
        onnx_path = os.path.join(args.output, "model.onnx")
        export_onnx(model, onnx_path)
        return

    # Load data
    if args.synthetic:
        features, labels = load_synthetic_data()
    elif args.data:
        features, labels = load_las_data(args.data)
    else:
        print("ERROR: Provide --data or --synthetic", file=sys.stderr)
        sys.exit(1)

    # Train/val split
    n = len(features)
    indices = np.random.permutation(n)
    val_n = int(n * args.val_split)
    val_idx, train_idx = indices[:val_n], indices[val_n:]

    train_ds = PointCloudDataset(features[train_idx], labels[train_idx], args.batch_size)
    train_loader = DataLoader(train_ds, batch_size=8, shuffle=True, num_workers=0)

    val_loader = None
    if val_n > 0:
        val_ds = PointCloudDataset(features[val_idx], labels[val_idx], args.batch_size)
        val_loader = DataLoader(val_ds, batch_size=8, shuffle=False, num_workers=0)

    # Print dataset stats
    unique, counts = np.unique(labels, return_counts=True)
    print("\nClass distribution:")
    for cls_idx, count in zip(unique, counts):
        code = CLASS_IDS[cls_idx] if cls_idx < len(CLASS_IDS) else -1
        name = CLASSES.get(code, "unknown")
        print(f"  {name} (code {code}): {count:,} points ({100*count/n:.1f}%)")

    # Train
    print(f"\nTraining {args.model} for {args.epochs} epochs...")
    history = train(model, train_loader, val_loader, args.epochs, args.lr, device, args.output)

    # Export to ONNX
    onnx_path = os.path.join(args.output, "model.onnx")
    model.cpu()
    export_onnx(model, onnx_path)

    # Summary
    print(f"\n{'='*60}")
    print(f"Training complete!")
    print(f"  Best model: {args.output}/best_model.pt")
    print(f"  ONNX model: {onnx_path}")
    print(f"  History:    {args.output}/history.json")
    print(f"\nTo deploy in TileTopia:")
    print(f"  1. Register: POST /api/v1/models with artifact_path={onnx_path}")
    print(f"  2. Set default: PUT /api/v1/models/{{id}}/default")
    print(f"  3. Or use PyTorch sidecar: python ml_classifier.py --weights {args.output}/best_model.pt")
    print(f"{'='*60}")


if __name__ == "__main__":
    main()

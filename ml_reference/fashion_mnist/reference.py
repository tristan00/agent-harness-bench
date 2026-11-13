import gzip
import hashlib
import json
import sys
import time
from pathlib import Path

import numpy as np
import pandas as pd
import scipy
import sklearn
from sklearn.ensemble import RandomForestClassifier
from sklearn.metrics import accuracy_score

SEED = 0


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def read_idx(path):
    with gzip.open(path, "rb") as f:
        data = f.read()
    magic = int.from_bytes(data[0:4], "big")
    if magic >> 8 != 0x08:
        raise ValueError(f"unexpected IDX dtype in {path}")
    ndim = magic & 0xFF
    dims = [int.from_bytes(data[4 + 4 * i:8 + 4 * i], "big") for i in range(ndim)]
    arr = np.frombuffer(data, dtype=np.uint8, offset=4 + 4 * ndim)
    return arr.reshape(dims)


def main():
    start = time.perf_counter()
    data_dir = Path(sys.argv[1])
    files = {
        "train_images": data_dir / "train-images-idx3-ubyte.gz",
        "train_labels": data_dir / "train-labels-idx1-ubyte.gz",
        "test_images": data_dir / "t10k-images-idx3-ubyte.gz",
        "test_labels": data_dir / "t10k-labels-idx1-ubyte.gz",
    }

    X_train = read_idx(files["train_images"]).reshape(-1, 28 * 28).astype(np.float32) / 255.0
    y_train = read_idx(files["train_labels"]).astype(np.int64)
    X_test = read_idx(files["test_images"]).reshape(-1, 28 * 28).astype(np.float32) / 255.0
    y_test = read_idx(files["test_labels"]).astype(np.int64)
    assert X_train.shape == (60000, 784) and y_train.shape == (60000,)
    assert X_test.shape == (10000, 784) and y_test.shape == (10000,)

    model = RandomForestClassifier(n_estimators=300, n_jobs=-1, random_state=SEED)
    model.fit(X_train, y_train)
    reference_score = float(accuracy_score(y_test, model.predict(X_test)))

    majority = np.bincount(y_train).argmax()
    trivial_score = float(np.mean(y_test == majority))

    result = {
        "dataset": "fashion_mnist",
        "metric": "accuracy",
        "reference_score": reference_score,
        "trivial_score": trivial_score,
        "model": "RandomForestClassifier(n_estimators=300, random_state=0) on raw 784 pixel intensities scaled to [0,1]",
        "train_rows": int(X_train.shape[0]),
        "test_rows": int(X_test.shape[0]),
        "train_sha256": {
            "train-images-idx3-ubyte.gz": sha256(files["train_images"]),
            "train-labels-idx1-ubyte.gz": sha256(files["train_labels"]),
        },
        "test_sha256": {
            "t10k-images-idx3-ubyte.gz": sha256(files["test_images"]),
            "t10k-labels-idx1-ubyte.gz": sha256(files["test_labels"]),
        },
        "runtime_seconds": round(time.perf_counter() - start, 2),
        "packages": {
            "python": sys.version.split()[0],
            "numpy": np.__version__,
            "pandas": pd.__version__,
            "scikit-learn": sklearn.__version__,
            "scipy": scipy.__version__,
        },
    }
    out = Path(__file__).resolve().parent / "result.json"
    out.write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()

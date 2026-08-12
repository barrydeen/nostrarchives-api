"""Model loading and scoring for the nspam classifier.

Downloads the model from the Hugging Face Hub (pinned by revision), loads the
LightGBM booster and the isotonic calibration table, and scores author bundles.
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Sequence

import lightgbm as lgb
import numpy as np

from features import build_matrix, build_vectorizers

REPO_ID = "barrydeen/nspam"
MODEL_DIR = "v2.2"

# Pin to a commit SHA, never a branch. A silent upstream retrain that changes
# the model under us is exactly the failure mode that produces an unexplainable
# ban wave. Override only when deliberately upgrading, and re-run the parity
# gate afterwards.
PINNED_REVISION = os.environ.get(
    "NSPAM_REVISION", "0110086dd1f7f2844b72827c2959fbc6c5e67b2d"
)

_FILES = [
    "config.json",
    "model.txt",
    "calibration.npz",
    "parity_fixtures.jsonl",
    "hash_fixtures.jsonl",
]


def fetch_model(revision: str = PINNED_REVISION) -> Path:
    """Download (or reuse the cache for) the model files. Returns the v2.2 dir."""
    from huggingface_hub import hf_hub_download

    local = None
    for name in _FILES:
        p = hf_hub_download(REPO_ID, f"{MODEL_DIR}/{name}", revision=revision)
        local = Path(p).parent
    assert local is not None
    return local


@dataclass
class Nspam:
    cfg: dict[str, Any]
    booster: lgb.Booster
    calib_x: np.ndarray
    calib_y: np.ndarray
    model_dir: Path

    @property
    def model_version(self) -> str:
        return self.cfg["model_version"]

    @property
    def total_features(self) -> int:
        return self.booster.num_feature()

    @classmethod
    def load(cls, model_dir: Path | None = None) -> "Nspam":
        d = Path(model_dir) if model_dir else fetch_model()
        cfg = json.loads((d / "config.json").read_text())
        booster = lgb.Booster(model_file=str(d / "model.txt"))

        z = np.load(d / "calibration.npz")
        cx = np.asarray(z["calib_x"], dtype=np.float64)
        cy = np.asarray(z["calib_y"], dtype=np.float64)
        if np.any(np.diff(cx) < 0):
            raise ValueError("calibration knots are not non-decreasing")

        expected = cfg["n_features_char"] + cfg["n_features_word"] + 23
        if booster.num_feature() != expected:
            raise ValueError(
                f"booster width {booster.num_feature()} != config-derived {expected}"
            )
        return cls(cfg=cfg, booster=booster, calib_x=cx, calib_y=cy, model_dir=d)

    def calibrate(self, raw: np.ndarray) -> np.ndarray:
        """Isotonic calibration: piecewise-linear interp, clamped to [0,1].

        np.interp already clamps to the endpoint values outside the knot range,
        matching sklearn's IsotonicRegression(out_of_bounds="clip").
        """
        return np.clip(np.interp(raw, self.calib_x, self.calib_y), 0.0, 1.0)

    def score(
        self, bundles: Sequence[Sequence[dict[str, Any]]]
    ) -> tuple[np.ndarray, np.ndarray]:
        """Score author bundles. Returns (raw_scores, calibrated_scores)."""
        if not bundles:
            return np.array([]), np.array([])
        char_vec, word_vec = build_vectorizers(self.cfg)
        X = build_matrix(bundles, char_vec, word_vec, self.total_features)
        raw = self.booster.predict(X)
        return np.asarray(raw, dtype=np.float64), self.calibrate(raw)

"""Scaled dot-product attention, one query row at a time. Correct, and slow on purpose."""

import math

import torch


def attention(q: torch.Tensor, k: torch.Tensor, v: torch.Tensor) -> torch.Tensor:
    batch, heads, seq, dim = q.shape
    out = torch.empty_like(q)
    scale = 1.0 / math.sqrt(dim)
    for b in range(batch):
        for h in range(heads):
            for i in range(seq):
                scores = (k[b, h] @ q[b, h, i]) * scale
                weights = torch.exp(scores - scores.max())
                weights = weights / weights.sum()
                out[b, h, i] = weights @ v[b, h]
    return out

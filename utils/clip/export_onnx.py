"""Exports a Hugging Face CLIP model's two halves to ONNX for the `clip` crate.

    pip install torch transformers onnx  # CPU torch is enough
    python export_onnx.py [REPO] [OUT_DIR]

Defaults: wkcn/TinyCLIP-ViT-8M-16-Text-3M-YFCC15M into
~/.local/share/archive-studio/models/tinyclip-8m. Writes:

- visual.onnx:  pixel_values f32 [1, 3, 224, 224] -> image_embeds f32 [1, 512]
- textual.onnx: input_ids i64 [1, 77] -> text_embeds f32 [1, 512]

Both outputs are the projected, not yet normalised, embeddings. A model with another
image size, context length or temperature needs the constants in the crate changed too.
"""

import os
import sys

import torch
from transformers import CLIPModel

repo = sys.argv[1] if len(sys.argv) > 1 else "wkcn/TinyCLIP-ViT-8M-16-Text-3M-YFCC15M"
out = sys.argv[2] if len(sys.argv) > 2 else os.path.expanduser("~/.local/share/archive-studio/models/tinyclip-8m")
os.makedirs(out, exist_ok=True)

model = CLIPModel.from_pretrained(repo).eval()
print(f"{repo}: logit scale {model.logit_scale.exp().item():.2f}, image size {model.config.vision_config.image_size}")


def features(output):
    # Newer transformers return a model output, older ones the tensor.
    return getattr(output, "pooler_output", output)


class Half(torch.nn.Module):
    """The model as a submodule, so tracing sees its weights."""

    def __init__(self):
        super().__init__()
        self.model = model


class Visual(Half):
    def forward(self, pixel_values):
        return features(self.model.get_image_features(pixel_values=pixel_values))


class Textual(Half):
    def forward(self, input_ids):
        return features(self.model.get_text_features(input_ids=input_ids))


size = model.config.vision_config.image_size
pixels = torch.randn(1, 3, size, size)
# Start of text, "a", end of text, then padding.
ids = torch.zeros(1, 77, dtype=torch.long)
ids[0, :3] = torch.tensor([49406, 320, 49407])
with torch.no_grad():
    torch.onnx.export(Visual(), (pixels,), f"{out}/visual.onnx", input_names=["pixel_values"],
                      output_names=["image_embeds"], opset_version=17, dynamo=False)
    torch.onnx.export(Textual(), (ids,), f"{out}/textual.onnx", input_names=["input_ids"],
                      output_names=["text_embeds"], opset_version=17, dynamo=False)
print(f"wrote {out}/visual.onnx and {out}/textual.onnx")

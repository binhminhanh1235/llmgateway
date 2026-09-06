#!/usr/bin/env node
import fs from "node:fs";
import assert from "node:assert/strict";

const app = fs.readFileSync("ui/app.js", "utf8");
const html = fs.readFileSync("ui/index.html", "utf8");
const css = fs.readFileSync("ui/app.css", "utf8");

const requireText = (source, needle, message) => {
  assert.ok(source.includes(needle), message + "\nMissing: " + needle);
};

requireText(html, 'id="imageFileInput"', "Chat composer must expose an image file picker");
requireText(html, 'accept="image/*"', "Image picker must constrain selection to image files");
requireText(html, 'id="attachImageButton"', "Chat composer must expose an attach-image control");
requireText(html, 'id="attachmentPreview"', "Chat composer must expose attachment previews");
requireText(html, 'id="composerDropZone"', "Chat composer must expose a drop target");

requireText(app, 'function selectedModelSupportsImages()', "UI must gate image attachment by model capabilities");
requireText(app, 'inputs.includes("image")', "UI must read structured image input capability");
requireText(app, 'function addPendingImages(files)', "UI must support queued image attachments");
requireText(app, 'apiFetch("/v1/files"', "UI image selection must upload through the Files API");
requireText(app, '{ type: "input_image", file_id: item.fileId }', "Thread requests must carry stable file_id references");
requireText(app, 'addEventListener("drop"', "UI must support drag/drop image input");
requireText(app, 'addEventListener("paste"', "UI must support pasted screenshot/image input");
requireText(app, 'function removePendingImage(localId)', "UI must allow removing a queued image");
requireText(app, 'URL.revokeObjectURL', "UI must release local preview object URLs");
requireText(app, 'The selected model does not support image input.', "UI must reject send when the selected model loses image capability");

requireText(css, ".attachment-preview", "Attachment preview layout must be styled");
requireText(css, ".attachment-chip", "Attachment preview chips must be styled");
requireText(css, ".composer.drag-active", "Drag-over state must be visible");
requireText(css, ".attach-button:disabled", "Capability-disabled attach state must be styled");

console.log("vision composer UI contract passed");

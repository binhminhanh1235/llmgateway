#!/usr/bin/env node
import fs from "node:fs";
import assert from "node:assert/strict";

const app = fs.readFileSync("ui/app.js", "utf8");
const html = fs.readFileSync("ui/index.html", "utf8");
const css = fs.readFileSync("ui/app.css", "utf8");

const requireText = (source, needle, message) => {
  assert.ok(source.includes(needle), message + "\nMissing: " + needle);
};

requireText(html, 'id="documentFileInput"', "Composer must expose a document file picker");
requireText(html, '.pdf,.txt,.md,.markdown,.docx,.csv,.json', "Document picker must constrain supported formats");
requireText(html, 'id="attachFileButton"', "Composer must expose a document attach control");

requireText(app, 'pendingFiles: []', "UI must track document uploads separately from image previews");
requireText(app, 'function selectedModelSupportsFiles()', "UI must read file-input model capability");
requireText(app, 'selectedModelInputModalities().includes("file")', "UI must gate native documents by structured file capability");
requireText(app, 'function isNativeDocument(file)', "UI must distinguish native PDF/DOCX from extraction fallback files");
requireText(app, 'function addPendingFiles(files)', "UI must queue supported document files");
requireText(app, 'uploadPendingAttachment(item, "assistants")', "Document selection must upload through Files API");
requireText(app, '{ type: "input_file", file_id: item.fileId }', "Thread requests must carry stable input_file file_id references");
requireText(app, 'text extraction fallback', "UI must explain extraction fallback state");
requireText(app, 'PDF/DOCX need a model with file input', "UI must reject unsupported native file capability");
requireText(app, 'addPendingFiles(dropped)', "Drag/drop must accept supported document files");
requireText(app, 'function removePendingFile(localId)', "UI must allow queued document removal");
requireText(app, 'clearPendingAttachmentsAfterSend()', "Successful send must clear image and document queues");

requireText(css, ".attachment-file-icon", "Document attachment chips must have a non-image visual");

console.log("file attachment composer UI contract passed");

// A square crop for profile pictures and group icons: the image with a movable,
// resizable square over it, like other sites. Only the chosen square is uploaded.
import { $, modal } from "./core.js";
import { html } from "./html.js";
import { Button } from "./ui.js";

/** The finished picture's width and height in pixels. */
const OUTPUT = 512;
/** The smallest square people can choose, in pixels of the original image. */
const SMALLEST = 32;

/**
 * Shows `file` with a square to move and resize. Resolves a square PNG Blob, or
 * null when the person cancels or the file isn't an image the browser can read.
 * The dialog stays open on success so the caller can show the upload's progress.
 */
export async function cropSquare(file, { title = "Crop your picture" } = {}) {
  const url = URL.createObjectURL(file);
  try {
    const image = await load(url);
    return await chooseSquare(image, title);
  } finally {
    URL.revokeObjectURL(url);
  }
}

function load(url) {
  return new Promise((resolve, reject) => {
    const image = new Image();
    image.decoding = "async";
    image.onload = () => image.naturalWidth && image.naturalHeight ? resolve(image) : reject(new Error("That image is empty."));
    image.onerror = () => reject(new Error("That file isn't an image this browser can open. Choose a PNG, JPEG or WebP image."));
    image.src = url;
  });
}

function chooseSquare(image, title) {
  modal(title, html`<p class="help">Drag the square to choose what shows, and drag a corner to resize it.</p>
<div class="cropper"><div class="cropper-stage"><img class="cropper-image" alt="" draggable="false"><div class="cropper-box" tabindex="0" role="slider" aria-label="Cropped area. Use the arrow keys to move it, and plus or minus to resize it." aria-valuetext="">
<span class="cropper-grid"></span><span class="cropper-handle" data-corner="nw"></span><span class="cropper-handle" data-corner="ne"></span><span class="cropper-handle" data-corner="sw"></span><span class="cropper-handle" data-corner="se"></span></div></div>
<div class="cropper-preview"><canvas width="96" height="96" aria-label="Preview"></canvas><span class="help">Preview</span></div></div>
<div class="form-footer align-end">${Button({ label: "Cancel", attrs: { "data-crop": "cancel" } })}${Button({ label: "Use this picture", kind: "primary", attrs: { "data-crop": "save" } })}</div>`);

  const body = $("#dialog-body"), dialog = $("#dialog");
  const stage = $(".cropper-stage", body), shown = $(".cropper-image", body), box = $(".cropper-box", body);
  const preview = $(".cropper-preview canvas", body).getContext("2d");
  shown.src = image.src;
  const width = image.naturalWidth, height = image.naturalHeight;
  // The square, in pixels of the original image. It starts as the largest centred one.
  const side = Math.min(width, height);
  let crop = { x: (width - side) / 2, y: (height - side) / 2, size: side };
  // Original pixels per pixel on screen.
  const scale = () => width / (shown.clientWidth || stage.clientWidth || width);

  function clamp(next) {
    const size = Math.max(Math.min(SMALLEST, side), Math.min(next.size, width, height));
    return { size, x: Math.min(Math.max(0, next.x), width - size), y: Math.min(Math.max(0, next.y), height - size) };
  }
  function draw() {
    const k = scale();
    box.style.left = `${crop.x / k}px`;
    box.style.top = `${crop.y / k}px`;
    box.style.width = box.style.height = `${crop.size / k}px`;
    box.setAttribute("aria-valuetext", `${Math.round(crop.size)} pixels square, ${Math.round(crop.x)} from the left and ${Math.round(crop.y)} from the top`);
    preview.imageSmoothingQuality = "high";
    preview.clearRect(0, 0, 96, 96);
    preview.drawImage(image, crop.x, crop.y, crop.size, crop.size, 0, 0, 96, 96);
  }

  // Dragging inside the square moves it; dragging a corner resizes it, keeping the
  // opposite corner where it is.
  let drag = null;
  box.addEventListener("pointerdown", event => {
    event.preventDefault();
    box.focus();
    try { box.setPointerCapture(event.pointerId); } catch { /* The pointer already ended. */ }
    drag = { corner: event.target.dataset.corner || null, startX: event.clientX, startY: event.clientY, from: { ...crop } };
  });
  box.addEventListener("pointermove", event => {
    if (!drag) return;
    const k = scale(), dx = (event.clientX - drag.startX) * k, dy = (event.clientY - drag.startY) * k, from = drag.from;
    if (!drag.corner) {
      crop = clamp({ ...from, x: from.x + dx, y: from.y + dy });
    } else {
      const west = drag.corner.includes("w"), north = drag.corner.includes("n");
      // Grow by whichever way the pointer moved further, limited by the image's edge.
      const grow = Math.max(west ? -dx : dx, north ? -dy : dy);
      const right = from.x + from.size, bottom = from.y + from.size;
      const room = Math.min(west ? right : width - from.x, north ? bottom : height - from.y);
      const size = Math.min(room, Math.max(Math.min(SMALLEST, side), from.size + grow));
      crop = { size, x: west ? right - size : from.x, y: north ? bottom - size : from.y };
    }
    draw();
  });
  const stop = () => { drag = null; };
  box.addEventListener("pointerup", stop);
  box.addEventListener("pointercancel", stop);
  box.addEventListener("keydown", event => {
    const step = Math.max(1, side / 50);
    const moves = { ArrowLeft: [-step, 0, 0], ArrowRight: [step, 0, 0], ArrowUp: [0, -step, 0], ArrowDown: [0, step, 0], "+": [-step / 2, -step / 2, step], "=": [-step / 2, -step / 2, step], "-": [step / 2, step / 2, -step] };
    const move = moves[event.key];
    if (!move) return;
    event.preventDefault();
    crop = clamp({ x: crop.x + move[0], y: crop.y + move[1], size: crop.size + move[2] });
    draw();
  });
  const resized = new ResizeObserver(draw);
  resized.observe(shown);
  shown.addEventListener("load", draw, { once: true });
  draw();

  return new Promise(resolve => {
    const finish = value => {
      resized.disconnect();
      dialog.removeEventListener("close", cancelled);
      body.removeEventListener("click", clicked);
      resolve(value);
    };
    const cancelled = () => finish(null);
    const clicked = event => {
      const choice = event.target.closest("[data-crop]")?.dataset.crop;
      if (choice === "cancel") dialog.close();
      if (choice !== "save") return;
      const canvas = document.createElement("canvas");
      canvas.width = canvas.height = Math.min(OUTPUT, Math.round(crop.size)) || OUTPUT;
      const context = canvas.getContext("2d");
      context.imageSmoothingQuality = "high";
      context.drawImage(image, crop.x, crop.y, crop.size, crop.size, 0, 0, canvas.width, canvas.height);
      event.target.closest("button").disabled = true;
      canvas.toBlob(blob => finish(blob), "image/png");
    };
    dialog.addEventListener("close", cancelled);
    body.addEventListener("click", clicked);
  });
}

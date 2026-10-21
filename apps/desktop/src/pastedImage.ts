// A picture pasted into the task composer, made to fit what otterd takes with
// a message (crates/protocol/src/task.rs): at most MAX_IMAGES, 700 KB each and
// 2 MB in all, decoded. Each is kept under TARGET_BYTES so that four of them
// are under the total.

export interface PastedImage {
  /** `image/png`, `image/jpeg` or `image/webp`. */
  media_type: string;
  /** base64 */
  data: string;
}

export const MAX_IMAGES = 4;
/** What otterd stores as the words of a message of pictures alone (`PICTURES_ONLY`). */
export const PICTURES_ONLY = "(pictures, no words)";
/** What one picture is shrunk to: four of these are under otterd's 2 MB total. */
export const TARGET_BYTES = 500 * 1024;
/** The longest side a picture keeps: more than a model reads anyway. */
export const MAX_SIDE = 1600;
const TYPES = ["image/png", "image/jpeg", "image/webp"];
const MIN_SIDE = 200;
const JPEG_QUALITIES = [0.9, 0.8, 0.7, 0.6, 0.5];

/** `w`×`h` scaled down so the longest side is at most `max`; never up. */
export function fitWithin(w: number, h: number, max = MAX_SIDE): { width: number; height: number } {
  const scale = Math.min(1, max / Math.max(w, h));
  return { width: Math.max(1, Math.round(w * scale)), height: Math.max(1, Math.round(h * scale)) };
}

/** The clipboard's pictures, when it holds pictures and no text (text wins: a copied table has both). */
export function pastedFiles(data: DataTransfer | null): File[] {
  if (!data) return [];
  const items = [...data.items];
  if (items.some((i) => i.kind === "string" && i.type === "text/plain")) return [];
  return items
    .filter((i) => i.kind === "file" && i.type.startsWith("image/"))
    .map((i) => i.getAsFile())
    .filter((f): f is File => f !== null);
}

function encode(canvas: HTMLCanvasElement, type: string, quality?: number): Promise<Blob | null> {
  return new Promise((resolve) => canvas.toBlob(resolve, type, quality));
}

function base64(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result).split(",")[1] ?? "");
    r.onerror = () => reject(r.error);
    r.readAsDataURL(blob);
  });
}

/**
 * The picture as otterd takes it: kept as it is when it already fits,
 * otherwise scaled down and re-encoded (PNG, then JPEG at lower quality, then
 * smaller) until it is under [`TARGET_BYTES`]. Throws if it can't be made to fit.
 */
export async function prepareImage(file: Blob): Promise<PastedImage> {
  const bitmap = await createImageBitmap(file);
  const { width, height } = fitWithin(bitmap.width, bitmap.height);
  if (TYPES.includes(file.type) && file.size <= TARGET_BYTES && width === bitmap.width && height === bitmap.height) {
    bitmap.close?.();
    return { media_type: file.type, data: await base64(file) };
  }
  const canvas = document.createElement("canvas");
  let side = { width, height };
  for (;;) {
    canvas.width = side.width;
    canvas.height = side.height;
    const ctx = canvas.getContext("2d")!;
    const tries: [string, number | undefined][] = [["image/png", undefined], ...JPEG_QUALITIES.map((q): [string, number] => ["image/jpeg", q])];
    for (const [type, quality] of tries) {
      // A JPEG has no transparency (it would turn black): paint on white.
      ctx.clearRect(0, 0, side.width, side.height);
      if (type === "image/jpeg") {
        ctx.fillStyle = "#fff";
        ctx.fillRect(0, 0, side.width, side.height);
      }
      ctx.drawImage(bitmap, 0, 0, side.width, side.height);
      const blob = await encode(canvas, type, quality);
      // A browser that can't make the type gives PNG: say what it is.
      if (blob && blob.size <= TARGET_BYTES && TYPES.includes(blob.type)) {
        bitmap.close?.();
        return { media_type: blob.type, data: await base64(blob) };
      }
    }
    if (Math.max(side.width, side.height) <= MIN_SIDE) break;
    side = { width: Math.round(side.width * 0.75), height: Math.round(side.height * 0.75) };
  }
  bitmap.close?.();
  throw new Error("That picture is too large to send, even shrunk.");
}

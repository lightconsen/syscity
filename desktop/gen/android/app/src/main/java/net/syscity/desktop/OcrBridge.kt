package net.syscity.desktop

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.util.Base64
import app.tauri.plugin.JSObject
import com.google.mlkit.vision.common.InputImage
import com.google.mlkit.vision.text.TextRecognition
import com.google.mlkit.vision.text.TextRecognizer
import com.google.mlkit.vision.text.chinese.ChineseTextRecognizerOptions
import org.json.JSONArray
import org.json.JSONObject

/**
 * On-device OCR, for the screens the accessibility tree cannot describe.
 *
 * The phone build has no `vision` feature — no ONNX Runtime, no `image` crate —
 * so without this the only OCR engine in the product is the one on a desktop
 * driving the phone over ADB. That left a screen full of canvas-drawn text
 * (a game, a Flutter page with no labels, an image-heavy chat) unreadable on
 * the very device that has a perfectly good recogniser built in.
 *
 * The engine is ML Kit's **bundled** Chinese model
 * (`com.google.mlkit:text-recognition-chinese`): the model ships inside the
 * APK, so it works offline, in flight mode, and on devices without Google Play
 * services. The unbundled variants download a model through GMS on first use
 * and are the wrong choice here.
 *
 * The wire contract with `src/computer/platform/android/ocr.rs`:
 *
 * ```
 * in:  { "pngBase64": "<base64 of a PNG>" }
 * out: { "ok": true, "width": 1080, "height": 2400,
 *        "lines": [ { "text": "设置", "bounds": [x1, y1, x2, y2],
 *                     "confidence": 0.93 } ] }
 *   or { "ok": false, "code": "ocr_unavailable", "error": "..." }
 * ```
 *
 * Bounds are in the *bitmap's* pixels, which for a `screencap` are the device's
 * own — the same space [`UiElement.center`] uses, so a line can be tapped by
 * passing its centre straight back.
 */
object OcrBridge {

  /** The engine could not run at all — distinct from "this screen has no text". */
  const val UNAVAILABLE = "ocr_unavailable"

  /**
   * One recogniser for the process lifetime.
   *
   * Constructing it is cheap, but keeping one client means the model is bound
   * once instead of on every screen read. `close()` is never called: the app
   * process owns it for as long as it lives.
   */
  private val recognizer: TextRecognizer? by lazy {
    try {
      TextRecognition.getClient(ChineseTextRecognizerOptions.Builder().build())
    } catch (t: Throwable) {
      // A missing native library or a corrupted model surfaces here or on the
      // first `process`; either way the caller gets `ocr_unavailable`, never an
      // empty list.
      android.util.Log.w("SyscityBridge", "ML Kit OCR is unavailable", t)
      null
    }
  }

  /**
   * Read the text in `pngBase64` and hand the answer to [callback].
   *
   * Called from a worker thread (decoding and OCR both block); [callback] runs
   * on the ML Kit task's main-thread executor.
   */
  fun recognize(pngBase64: String, callback: (JSObject) -> Unit) {
    val engine = recognizer
    if (engine == null) {
      callback(failure("the on-device OCR engine could not be loaded"))
      return
    }

    val png = try {
      Base64.decode(pngBase64, Base64.DEFAULT)
    } catch (e: IllegalArgumentException) {
      callback(failure("the screenshot was not valid base64 (${e.message})"))
      return
    }

    // ML Kit takes a Bitmap, not encoded bytes: `InputImage.fromByteArray` only
    // accepts NV21/YV12/YUV_420_888, so a PNG has to be decoded first.
    val bitmap = try {
      BitmapFactory.decodeByteArray(png, 0, png.size)
    } catch (t: Throwable) {
      null
    }
    if (bitmap == null) {
      callback(failure("the screenshot could not be decoded as an image"))
      return
    }

    engine.process(InputImage.fromBitmap(bitmap, 0))
      .addOnSuccessListener { text -> callback(success(text, bitmap)) }
      .addOnFailureListener { e ->
        callback(failure(e.message ?: e.javaClass.simpleName))
        recycle(bitmap)
      }
  }

  private fun success(text: com.google.mlkit.vision.text.Text, bitmap: Bitmap): JSObject {
    val lines = JSONArray()
    for (block in text.textBlocks) {
      for (line in block.lines) {
        val content = line.text.trim()
        val box = line.boundingBox ?: continue
        // `OcrLine.bounds` is `(x1, y1, x2, y2)` and is tapped at its centre;
        // a zero-area box would put the tap on the box's own corner line.
        if (content.isEmpty() || box.width() <= 0 || box.height() <= 0) continue
        lines.put(
          JSONObject()
            .put("text", content)
            .put(
              "bounds",
              JSONArray(listOf(box.left, box.top, box.right, box.bottom)),
            )
            // -1 means "not reported": an absent confidence must reach Rust as
            // absent, not as a negative sentinel it would have to know about.
            .put("confidence", line.confidence.coerceAtLeast(0f).toDouble()),
        )
      }
    }
    val width = bitmap.width
    val height = bitmap.height
    // A screen-sized bitmap is 8–10 MB and this is the hot path, so it is
    // released before the answer travels back across the bridge.
    recycle(bitmap)
    return JSObject().put("ok", true).put("lines", lines).put("width", width).put("height", height)
  }

  private fun failure(message: String): JSObject = JSObject()
    .put("ok", false)
    .put("code", UNAVAILABLE)
    .put("error", message)

  /** Release a decoded frame; a second `recycle()` would throw. */
  private fun recycle(bitmap: Bitmap) {
    try {
      if (!bitmap.isRecycled) bitmap.recycle()
    } catch (t: Throwable) {
      android.util.Log.w("SyscityBridge", "could not recycle the OCR frame", t)
    }
  }
}

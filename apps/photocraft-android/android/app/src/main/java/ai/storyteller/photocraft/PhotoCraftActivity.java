package ai.storyteller.photocraft;

import android.app.NativeActivity;
import android.content.ClipData;
import android.content.ClipDescription;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.provider.OpenableColumns;
import android.util.SparseArray;
import android.view.MotionEvent;
import android.view.View;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.window.OnBackInvokedCallback;
import android.window.OnBackInvokedDispatcher;

import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.ArrayList;
import java.util.UUID;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * Android's small host around the native eframe editor. The Storage Access Framework is kept in
 * Java because it owns Activity results and URI permissions; all editing remains Rust.
 */
public final class PhotoCraftActivity extends NativeActivity {
    private static final int REQUEST_OPEN = 4101;
    private static final int REQUEST_SAVE = 4102;
    private static final long POLL_MS = 40L;
    private static final long MAX_IMPORT_BYTES = 1024L * 1024L * 1024L;
    private static final long TWO_FINGER_TAP_TIMEOUT_MS = 350L;

    static {
        // NativeActivity loads this library to enter android_main. Loading it from this class
        // also binds the Storage Access Framework JNI methods to this class's class loader.
        System.loadLibrary("photocraft_android");
    }

    private final Handler handler = new Handler(Looper.getMainLooper());
    private final ExecutorService io = Executors.newSingleThreadExecutor();
    private final SparseArray<float[]> touchStarts = new SparseArray<>();
    private ClipboardManager clipboardManager;
    private Uri lastCopiedUri;
    private final ClipboardManager.OnPrimaryClipChangedListener clipListener = this::checkClipboard;
    private boolean pickerOpen;
    private boolean twoFingerEligible;
    private boolean twoFingerMoved;
    private int maxTouchPointers;
    private long touchStartedAt;
    private float twoFingerCenterX;
    private float twoFingerCenterY;
    private float touchSlopPx;
    private boolean stylusTipDown;
    private boolean stylusBarrelHeld;
    private OnBackInvokedCallback backCallback;

    private final Runnable bridgePump = new Runnable() {
        @Override
        public void run() {
            dispatchPickerRequest();
            dispatchSave();
            dispatchTakeClipboardSetImage();
            handler.postDelayed(this, POLL_MS);
        }
    };

    private static native String nativeTakePickerRequest();
    private static native void nativePickerOpened(String cachePaths);
    private static native void nativePickerSaveTarget(String uri);
    private static native void nativePickerCancelled();
    private static native void nativePickerFailed(String error);
    private static native String nativeTakeSaveUri();
    private static native byte[] nativeTakeSaveBytes();
    private static native void nativeSaveFinished(boolean success, String error);
    private static native void nativeTwoFingerTap(float x, float y);
    private static native void nativeStylusBarrel(float x, float y, boolean pressed);
    private static native void nativeClipboardImageChanged(byte[] bytes);
    private static native void nativeClipboardImageCleared();
    private static native byte[] nativeTakeClipboardSetBytes();

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        touchSlopPx = android.view.ViewConfiguration.get(this).getScaledTouchSlop();
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            backCallback = () -> moveTaskToBack(true);
            getOnBackInvokedDispatcher().registerOnBackInvokedCallback(OnBackInvokedDispatcher.PRIORITY_DEFAULT, backCallback);
        }
        clipboardManager = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        if (clipboardManager != null) {
            clipboardManager.addPrimaryClipChangedListener(clipListener);
        }
        enterImmersiveMode();
        checkClipboard();
        handler.post(bridgePump);
    }

    @Override
    protected void onResume() {
        super.onResume();
        enterImmersiveMode();
        checkClipboard();
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) {
            enterImmersiveMode();
            checkClipboard();
        }
    }

    @Override
    protected void onDestroy() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU && backCallback != null) {
            getOnBackInvokedDispatcher().unregisterOnBackInvokedCallback(backCallback);
        }
        if (clipboardManager != null) {
            clipboardManager.removePrimaryClipChangedListener(clipListener);
        }
        handler.removeCallbacks(bridgePump);
        io.shutdownNow();
        super.onDestroy();
    }

    @Override
    @SuppressWarnings("deprecation")
    public void onBackPressed() {
        moveTaskToBack(true);
    }

    /** Keeps the editing surface clear of status and navigation bars. A swipe still reveals them temporarily. */
    @SuppressWarnings("deprecation")
    private void enterImmersiveMode() {
        Window window = getWindow();
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window.setDecorFitsSystemWindows(false);
            WindowInsetsController controller = window.getInsetsController();
            if (controller != null) {
                controller.hide(WindowInsets.Type.statusBars() | WindowInsets.Type.navigationBars());
                controller.setSystemBarsBehavior(WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
            }
            return;
        }
        window.getDecorView().setSystemUiVisibility(
            View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                | View.SYSTEM_UI_FLAG_FULLSCREEN
                | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                | View.SYSTEM_UI_FLAG_LAYOUT_STABLE
        );
    }

    @Override
    public boolean dispatchTouchEvent(MotionEvent event) {
        // Two-finger taps are recognized from Rust's native input queue, which also receives
        // touches when NativeActivity bypasses Java dispatchTouchEvent.
        trackStylusBarrel(event);
        return super.dispatchTouchEvent(event);
    }

    private void trackTwoFingerTap(MotionEvent event) {
        int action = event.getActionMasked();
        int index = event.getActionIndex();
        if (action == MotionEvent.ACTION_DOWN) {
            touchStarts.clear();
            twoFingerEligible = true;
            twoFingerMoved = false;
            maxTouchPointers = 1;
            touchStartedAt = event.getEventTime();
            recordTouchStart(event, 0);
            return;
        }
        if (action == MotionEvent.ACTION_POINTER_DOWN) {
            maxTouchPointers = Math.max(maxTouchPointers, event.getPointerCount());
            if (maxTouchPointers > 2) {
                twoFingerEligible = false;
            }
            recordTouchStart(event, index);
            if (maxTouchPointers == 2 && touchStarts.size() >= 2) {
                float[] first = touchStarts.valueAt(0);
                float[] second = touchStarts.valueAt(1);
                twoFingerCenterX = (first[0] + second[0]) * 0.5f;
                twoFingerCenterY = (first[1] + second[1]) * 0.5f;
            }
            return;
        }
        if (action == MotionEvent.ACTION_MOVE) {
            for (int pointer = 0; pointer < event.getPointerCount(); pointer++) {
                float[] start = touchStarts.get(event.getPointerId(pointer));
                if (start == null) {
                    twoFingerEligible = false;
                    continue;
                }
                float dx = event.getX(pointer) - start[0];
                float dy = event.getY(pointer) - start[1];
                if (dx * dx + dy * dy > touchSlopPx * touchSlopPx) {
                    twoFingerMoved = true;
                }
            }
            return;
        }
        if (action == MotionEvent.ACTION_UP) {
            if (twoFingerEligible && maxTouchPointers == 2 && !twoFingerMoved && event.getEventTime() - touchStartedAt <= TWO_FINGER_TAP_TIMEOUT_MS) {
                nativeTwoFingerTap(normalizedX(twoFingerCenterX), normalizedY(twoFingerCenterY));
            }
            touchStarts.clear();
            return;
        }
        if (action == MotionEvent.ACTION_CANCEL) {
            touchStarts.clear();
        }
    }

    private void recordTouchStart(MotionEvent event, int index) {
        if (event.getToolType(index) != MotionEvent.TOOL_TYPE_FINGER) {
            twoFingerEligible = false;
        }
        touchStarts.put(event.getPointerId(index), new float[] {event.getX(index), event.getY(index)});
    }

    private void trackStylusBarrel(MotionEvent event) {
        int pointer = stylusPointerIndex(event);
        if (pointer < 0) {
            return;
        }
        int action = event.getActionMasked();
        boolean barrelNow = (event.getButtonState() & (MotionEvent.BUTTON_STYLUS_PRIMARY | MotionEvent.BUTTON_STYLUS_SECONDARY)) != 0;
        float x = normalizedX(event.getX(pointer));
        float y = normalizedY(event.getY(pointer));
        if (action == MotionEvent.ACTION_DOWN) {
            stylusTipDown = true;
            stylusBarrelHeld = barrelNow;
            if (barrelNow) {
                nativeStylusBarrel(x, y, true);
            }
            return;
        }
        if (action == MotionEvent.ACTION_UP || action == MotionEvent.ACTION_CANCEL) {
            if (stylusTipDown && stylusBarrelHeld) {
                nativeStylusBarrel(x, y, false);
            }
            stylusTipDown = false;
            stylusBarrelHeld = false;
            return;
        }
        if (stylusTipDown && barrelNow != stylusBarrelHeld) {
            stylusBarrelHeld = barrelNow;
            nativeStylusBarrel(x, y, barrelNow);
        }
    }

    private int stylusPointerIndex(MotionEvent event) {
        for (int index = 0; index < event.getPointerCount(); index++) {
            int toolType = event.getToolType(index);
            if (toolType == MotionEvent.TOOL_TYPE_STYLUS || toolType == MotionEvent.TOOL_TYPE_ERASER) {
                return index;
            }
        }
        return -1;
    }

    private float normalizedX(float x) {
        View root = getWindow().getDecorView();
        int width = root.getWidth();
        if (width <= 0) {
            width = getResources().getDisplayMetrics().widthPixels;
        }
        return width > 0 ? Math.max(0.0f, Math.min(1.0f, x / width)) : 0.0f;
    }

    private float normalizedY(float y) {
        View root = getWindow().getDecorView();
        int height = root.getHeight();
        if (height <= 0) {
            height = getResources().getDisplayMetrics().heightPixels;
        }
        return height > 0 ? Math.max(0.0f, Math.min(1.0f, y / height)) : 0.0f;
    }

    private void dispatchPickerRequest() {
        if (pickerOpen) {
            return;
        }
        String request = nativeTakePickerRequest();
        if (request == null || request.isEmpty()) {
            return;
        }
        if (request.startsWith("open:")) {
            pickerOpen = true;
            Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
            intent.addCategory(Intent.CATEGORY_OPENABLE);
            intent.setType("*/*");
            intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, request.equals("open:multiple"));
            intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION);
            startActivityForResult(intent, REQUEST_OPEN);
            return;
        }
        if (request.startsWith("save:")) {
            pickerOpen = true;
            Intent intent = new Intent(Intent.ACTION_CREATE_DOCUMENT);
            intent.addCategory(Intent.CATEGORY_OPENABLE);
            intent.setType("application/octet-stream");
            intent.putExtra(Intent.EXTRA_TITLE, request.substring("save:".length()));
            intent.addFlags(Intent.FLAG_GRANT_WRITE_URI_PERMISSION | Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION);
            startActivityForResult(intent, REQUEST_SAVE);
            return;
        }
        nativePickerFailed("PhotoCraft received an invalid Android document-picker request.");
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent result) {
        super.onActivityResult(requestCode, resultCode, result);
        if (requestCode != REQUEST_OPEN && requestCode != REQUEST_SAVE) {
            return;
        }
        pickerOpen = false;
        if (resultCode != RESULT_OK || result == null) {
            nativePickerCancelled();
            return;
        }
        Uri uri = result.getData();
        if (requestCode == REQUEST_SAVE) {
            if (uri == null) {
                nativePickerCancelled();
            } else {
                takePersistablePermission(uri, Intent.FLAG_GRANT_WRITE_URI_PERMISSION);
                nativePickerSaveTarget(uri.toString());
            }
            return;
        }
        ArrayList<Uri> documents = new ArrayList<>();
        if (result.getClipData() != null) {
            for (int i = 0; i < result.getClipData().getItemCount(); i++) {
                Uri item = result.getClipData().getItemAt(i).getUri();
                if (item != null) {
                    documents.add(item);
                }
            }
        } else if (uri != null) {
            documents.add(uri);
        }
        if (documents.isEmpty()) {
            nativePickerCancelled();
            return;
        }
        io.execute(() -> copyOpenedDocuments(documents));
    }

    private void copyOpenedDocuments(ArrayList<Uri> documents) {
        try {
            ArrayList<String> paths = new ArrayList<>();
            for (Uri uri : documents) {
                takePersistablePermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION);
                paths.add(copyToCache(uri).getAbsolutePath());
            }
            nativePickerOpened(joinLines(paths));
        } catch (Exception error) {
            nativePickerFailed("Could not read the selected document: " + message(error));
        }
    }

    private void dispatchSave() {
        String uri = nativeTakeSaveUri();
        if (uri == null || uri.isEmpty()) {
            return;
        }
        byte[] bytes = nativeTakeSaveBytes();
        if (bytes == null) {
            nativeSaveFinished(false, "PhotoCraft could not prepare document data for Android.");
            return;
        }
        io.execute(() -> writeDocument(uri, bytes));
    }

    private void writeDocument(String uriText, byte[] bytes) {
        try (OutputStream output = getContentResolver().openOutputStream(Uri.parse(uriText), "wt")) {
            if (output == null) {
                throw new IOException("The selected document provider returned no output stream.");
            }
            output.write(bytes);
            output.flush();
            nativeSaveFinished(true, "");
        } catch (Exception error) {
            nativeSaveFinished(false, "Could not write the selected document: " + message(error));
        }
    }

    private File copyToCache(Uri uri) throws IOException {
        File directory = new File(new File(getCacheDir(), "imports"), UUID.randomUUID().toString());
        if (!directory.mkdirs()) {
            throw new IOException("Could not create a private import directory.");
        }
        File destination = new File(directory, safeFileName(displayName(uri)));
        long copied = 0L;
        byte[] buffer = new byte[1024 * 1024];
        try (InputStream input = getContentResolver().openInputStream(uri); OutputStream output = new FileOutputStream(destination)) {
            if (input == null) {
                throw new IOException("The selected document provider returned no input stream.");
            }
            int read;
            while ((read = input.read(buffer)) != -1) {
                copied += read;
                if (copied > MAX_IMPORT_BYTES) {
                    throw new IOException("The selected document is larger than PhotoCraft's 1 GiB Android import limit.");
                }
                output.write(buffer, 0, read);
            }
            output.flush();
        } catch (IOException error) {
            // A partial import must never become an apparently valid source path.
            if (!destination.delete()) {
                destination.deleteOnExit();
            }
            throw error;
        }
        return destination;
    }

    private String displayName(Uri uri) {
        try (Cursor cursor = getContentResolver().query(uri, new String[] {OpenableColumns.DISPLAY_NAME}, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                int index = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                if (index >= 0) {
                    String name = cursor.getString(index);
                    if (name != null && !name.isEmpty()) {
                        return name;
                    }
                }
            }
        } catch (Exception ignored) {
            // The display name is optional; a safe generic name preserves the import itself.
        }
        return "import";
    }

    private void takePersistablePermission(Uri uri, int flags) {
        try {
            getContentResolver().takePersistableUriPermission(uri, flags);
        } catch (SecurityException ignored) {
            // Some providers grant a one-shot URI only. The private import copy or immediate save
            // still works, and PhotoCraft never assumes the grant survived a provider refusal.
        }
    }

    private static String safeFileName(String name) {
        String cleaned = name.replaceAll("[^A-Za-z0-9._-]", "_");
        if (cleaned.isEmpty() || cleaned.replace(".", "").isEmpty()) {
            return "import";
        }
        return cleaned.length() > 120 ? cleaned.substring(0, 120) : cleaned;
    }

    private static String joinLines(ArrayList<String> paths) {
        StringBuilder result = new StringBuilder();
        for (String path : paths) {
            if (result.length() > 0) {
                result.append('\n');
            }
            result.append(path);
        }
        return result.toString();
    }

    private static String message(Exception error) {
        String detail = error.getMessage();
        return detail == null || detail.isEmpty() ? error.getClass().getSimpleName() : detail;
    }

    private void checkClipboard() {
        if (clipboardManager == null || !clipboardManager.hasPrimaryClip()) {
            nativeClipboardImageCleared();
            return;
        }
        ClipData clip = clipboardManager.getPrimaryClip();
        if (clip == null || clip.getItemCount() == 0) {
            nativeClipboardImageCleared();
            return;
        }
        Uri uri = null;
        for (int i = 0; i < clip.getItemCount(); i++) {
            ClipData.Item item = clip.getItemAt(i);
            if (item != null && item.getUri() != null) {
                uri = item.getUri();
                break;
            }
        }
        if (uri == null) {
            nativeClipboardImageCleared();
            return;
        }
        if (uri.equals(lastCopiedUri)) {
            return;
        }
        String type = getContentResolver().getType(uri);
        boolean isImage = (type != null && type.startsWith("image/"))
            || (clip.getDescription() != null && clip.getDescription().hasMimeType("image/*"));
        if (!isImage && uri.getPath() != null) {
            String path = uri.getPath().toLowerCase();
            isImage = path.endsWith(".png") || path.endsWith(".jpg") || path.endsWith(".jpeg")
                || path.endsWith(".webp") || path.endsWith(".bmp") || path.endsWith(".tiff");
        }
        if (!isImage) {
            nativeClipboardImageCleared();
            return;
        }
        final Uri finalUri = uri;
        io.execute(() -> {
            try (InputStream is = getContentResolver().openInputStream(finalUri)) {
                if (is == null) {
                    nativeClipboardImageCleared();
                    return;
                }
                ByteArrayOutputStream baos = new ByteArrayOutputStream();
                byte[] buffer = new byte[64 * 1024];
                int read;
                long total = 0;
                while ((read = is.read(buffer)) != -1) {
                    total += read;
                    if (total > 64 * 1024 * 1024) {
                        break;
                    }
                    baos.write(buffer, 0, read);
                }
                byte[] bytes = baos.toByteArray();
                if (bytes.length > 0) {
                    nativeClipboardImageChanged(bytes);
                } else {
                    nativeClipboardImageCleared();
                }
            } catch (Exception e) {
                nativeClipboardImageCleared();
            }
        });
    }

    private void dispatchTakeClipboardSetImage() {
        byte[] bytes = nativeTakeClipboardSetBytes();
        if (bytes == null || bytes.length == 0) {
            return;
        }
        io.execute(() -> {
            try {
                File dir = new File(getCacheDir(), "clipboard");
                if (!dir.exists()) {
                    dir.mkdirs();
                }
                File file = new File(dir, "clipboard.png");
                try (FileOutputStream fos = new FileOutputStream(file)) {
                    fos.write(bytes);
                    fos.flush();
                }
                Uri uri = Uri.parse("content://" + ImageProvider.AUTHORITY + "/clipboard.png");
                lastCopiedUri = uri;
                handler.post(() -> {
                    if (clipboardManager != null) {
                        ClipDescription desc = new ClipDescription("PhotoCraft Image", new String[] {"image/png"});
                        ClipData clip = new ClipData(desc, new ClipData.Item(uri));
                        clipboardManager.setPrimaryClip(clip);
                    }
                });
            } catch (Exception e) {
                // ignore
            }
        });
    }
}
